//! Outbound Velocity Modern Forwarding signer.
//!
//! PicoLimbo ships an **inbound** verifier
//! (`forwarding/check_velocity_key_integrity.rs`) for the case where
//! Velocity routes a player to us. This module is its mirror: it
//! constructs a *signed response* to the `velocity:player_info`
//! Login Plugin Request that a Forge backend will send us when we
//! pretend to be Velocity ourselves.
//!
//! ## Why we need this
//!
//! In a typical Forge network the topology is:
//!
//! ```text
//!     Client ── Velocity ── Forge backend #1
//!                       └── Forge backend #2
//!                       └── PicoLimbo (us)
//! ```
//!
//! Every backend (including PicoLimbo) is configured with
//! Modern Forwarding, sharing a single HMAC secret with Velocity.
//! When the recorder needs to connect to a sibling Forge backend to
//! record its handshake, that backend will reject any connection that
//! does not pass the Velocity-style HMAC check. To get past that gate
//! the recorder *itself* has to play the Velocity role outbound, using
//! the same shared secret PicoLimbo already has in its
//! `[forwarding] modern { secret }` config.
//!
//! ## Wire format (Velocity protocol v1)
//!
//! `signature(32B) || payload`, where `payload` is:
//!
//! 1. `VarInt` — version (`1` is the universally-supported minimum).
//! 2. `VarIntPrefixedString` — connecting player's IP address.
//! 3. `Uuid` (16B big-endian) — player UUID.
//! 4. `VarIntPrefixedString` — player username.
//! 5. `VarInt` — property count.
//! 6. For each property:
//!    * `VarIntPrefixedString` name
//!    * `VarIntPrefixedString` value
//!    * `bool` + optional `VarIntPrefixedString` signature
//!
//! Forge backends only care that the HMAC verifies — the actual values
//! never reach the Forge code; they get consumed by the Velocity
//! plugin that gates the connection. We can therefore pass synthetic
//! values (`127.0.0.1`, the nil UUID, a recorder username, zero
//! properties) and the server happily proceeds to the Forge handshake.

use hmac::digest::InvalidLength;
use hmac::{Hmac, KeyInit, Mac};
use minecraft_protocol::prelude::{BinaryWriter, BinaryWriterError, VarInt};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// The Velocity Modern Forwarding payload version we emit.
///
/// Velocity supports several versions (the server's `velocity:player_info`
/// LPR carries its *maximum* supported version in its 1-byte payload).
/// Version 1 is the baseline that every Velocity build accepts, and
/// avoids carrying chat-signing material we have no use for. The Forge
/// servers we have observed accept v1 even when they advertise v4.
pub const VELOCITY_FORWARDING_VERSION: i32 = 1;

#[derive(Debug, Error)]
pub enum VelocitySignError {
    #[error("invalid HMAC key length")]
    InvalidKey,
    #[error(transparent)]
    Encode(#[from] BinaryWriterError),
}

impl From<InvalidLength> for VelocitySignError {
    fn from(_: InvalidLength) -> Self {
        Self::InvalidKey
    }
}

/// Synthetic identity emitted in the Velocity forwarding payload by the
/// recorder. None of these values are inspected by Forge itself; they
/// just have to be syntactically valid.
pub struct OutboundIdentity<'a> {
    pub addr: &'a str,
    pub uuid: Uuid,
    pub username: &'a str,
}

impl<'a> OutboundIdentity<'a> {
    /// Builds a default identity suitable for recording sessions.
    /// `127.0.0.1` is sometimes blocked by IP rate-limiters in
    /// production Velocity deployments but is universally accepted by
    /// backend servers.
    pub fn recorder(username: &'a str) -> Self {
        Self {
            addr: "127.0.0.1",
            uuid: Uuid::nil(),
            username,
        }
    }
}

/// Builds the full *signed* payload that goes into the body of a
/// serverbound Login Plugin Response acknowledging the upstream's
/// `velocity:player_info` Login Plugin Request.
///
/// The return value is the **complete plugin-response data**
/// (signature || payload). The caller wraps it in the standard LPR
/// envelope (`messageId + isPresent + data`).
pub fn build_signed_player_info(
    secret_key: &[u8],
    identity: &OutboundIdentity<'_>,
) -> Result<Vec<u8>, VelocitySignError> {
    let payload = build_payload(identity)?;

    let mut mac = HmacSha256::new_from_slice(secret_key)?;
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();

    // signature (32B) || payload
    let mut out = Vec::with_capacity(32 + payload.len());
    out.extend_from_slice(&signature);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn build_payload(identity: &OutboundIdentity<'_>) -> Result<Vec<u8>, VelocitySignError> {
    let mut writer = BinaryWriter::new();
    writer.write(&VarInt::new(VELOCITY_FORWARDING_VERSION))?;
    write_var_int_string(&mut writer, identity.addr)?;
    writer.write_bytes(identity.uuid.as_bytes())?;
    write_var_int_string(&mut writer, identity.username)?;
    // Property count = 0 (no textures / skin data for the synthetic
    // identity we send).
    writer.write(&VarInt::new(0))?;
    Ok(writer.into_inner())
}

fn write_var_int_string(
    writer: &mut BinaryWriter,
    value: &str,
) -> Result<(), BinaryWriterError> {
    let bytes = value.as_bytes();
    writer.write(&VarInt::new(i32::try_from(bytes.len())?))?;
    writer.write_bytes(bytes)?;
    Ok(())
}

/// True when the given Login Plugin Request channel is Velocity's
/// player-info probe. The check matches both the canonical channel
/// name `velocity:player_info` and a few historical variants.
pub fn is_velocity_player_info_channel(channel: &str) -> bool {
    matches!(channel, "velocity:player_info" | "velocity:player_info ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use minecraft_protocol::prelude::{BinaryReader, VarIntPrefixedString};

    fn parse_signed_payload(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        assert!(data.len() >= 32, "payload too short");
        (data[..32].to_vec(), data[32..].to_vec())
    }

    #[test]
    fn signed_payload_starts_with_32_byte_signature() {
        let identity = OutboundIdentity::recorder("Probe");
        let signed = build_signed_player_info(b"secret", &identity).unwrap();
        assert!(signed.len() > 32);
    }

    #[test]
    fn signature_verifies_with_same_secret() {
        let identity = OutboundIdentity::recorder("Probe");
        let secret = b"my-velocity-secret";
        let signed = build_signed_player_info(secret, &identity).unwrap();
        let (signature, payload) = parse_signed_payload(&signed);

        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(&payload);
        let expected = mac.finalize().into_bytes();
        assert_eq!(&signature[..], &expected[..]);
    }

    #[test]
    fn signature_differs_with_different_secret() {
        let identity = OutboundIdentity::recorder("Probe");
        let signed_a = build_signed_player_info(b"secret-a", &identity).unwrap();
        let signed_b = build_signed_player_info(b"secret-b", &identity).unwrap();
        assert_ne!(&signed_a[..32], &signed_b[..32], "HMAC signature must depend on secret");
        // Payload (post-signature) must be identical.
        assert_eq!(&signed_a[32..], &signed_b[32..]);
    }

    #[test]
    fn payload_decodes_to_expected_fields() {
        let identity = OutboundIdentity {
            addr: "10.0.0.1",
            uuid: Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef),
            username: "PicoRecorder",
        };
        let signed = build_signed_player_info(b"x", &identity).unwrap();
        let (_sig, payload) = parse_signed_payload(&signed);

        let mut reader = BinaryReader::new(&payload);
        let version = reader.read::<VarInt>().unwrap().inner();
        let addr: VarIntPrefixedString = reader.read().unwrap();
        let mut uuid_bytes = [0u8; 16];
        reader.read_bytes(&mut uuid_bytes).unwrap();
        let username: VarIntPrefixedString = reader.read().unwrap();
        let prop_count = reader.read::<VarInt>().unwrap().inner();

        assert_eq!(version, VELOCITY_FORWARDING_VERSION);
        assert_eq!(addr.into_inner(), "10.0.0.1");
        assert_eq!(
            uuid_bytes,
            *Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef).as_bytes()
        );
        assert_eq!(username.into_inner(), "PicoRecorder");
        assert_eq!(prop_count, 0);
    }

    #[test]
    fn is_velocity_player_info_channel_recognises_canonical_name() {
        assert!(is_velocity_player_info_channel("velocity:player_info"));
        assert!(!is_velocity_player_info_channel("fml:loginwrapper"));
        assert!(!is_velocity_player_info_channel("minecraft:brand"));
    }

    #[test]
    fn recorder_identity_uses_nil_uuid_and_localhost() {
        let id = OutboundIdentity::recorder("Bob");
        assert_eq!(id.addr, "127.0.0.1");
        assert_eq!(id.uuid, Uuid::nil());
        assert_eq!(id.username, "Bob");
    }
}
