//! Handler for the serverbound Configuration Plugin Message packet
//! (`0x02` in the Configuration state).
//!
//! Two distinct purposes:
//!
//! 1. **FML3 replay** — when a 1.20.2+ Forge / `NeoForge` client is in
//!    the middle of a recorded handshake, every inbound `fml:handshake`
//!    (or `neoforge:handshake`) reply advances the
//!    [`Fml3ReplaySession`] cursor and triggers the next outbound
//!    snapshot step. When the snapshot is exhausted the session is
//!    torn down and the limbo's normal Configuration packets
//!    (registry data, finish configuration) take over.
//! 2. **Vanilla brand acknowledgements** — non-Forge clients may send
//!    `minecraft:brand` here, which we silently ignore.

use crate::forge::replay::Fml3ReplaySession;
use crate::server::batch::Batch;
use crate::server::client_state::ClientState;
use crate::server::packet_handler::{PacketHandler, PacketHandlerError};
use crate::server::packet_registry::PacketRegistry;
use crate::server_state::ServerState;
use minecraft_packets::configuration::configuration_client_bound_plugin_message_packet::ConfigurationClientBoundPluginMessagePacket;
use minecraft_packets::configuration::configuration_server_bound_plugin_message_packet::ConfigurationServerBoundPluginMessagePacket;
use minecraft_protocol::prelude::Identifier;
use tracing::debug;

impl PacketHandler for ConfigurationServerBoundPluginMessagePacket {
    fn handle(
        &self,
        client_state: &mut ClientState,
        server_state: &ServerState,
    ) -> Result<Batch<PacketRegistry>, PacketHandlerError> {
        let mut batch = Batch::new();

        // Only `fml:*` / `neoforge:*` channels are relevant to the
        // Forge replay state machine. Vanilla channels (e.g. brand)
        // pass through as no-ops.
        let chan = self.channel.to_string();
        if !is_forge_handshake_channel(&chan) {
            debug!(
                "configuration plugin message on non-forge channel `{}` ignored",
                chan
            );
            return Ok(batch);
        }

        // Advance the FML3 replay session (if any). Each inbound
        // forge plugin message means "I've processed your last
        // request, give me the next one".
        let snapshot = server_state.forge_snapshot();
        let fml3 = snapshot.as_ref().and_then(|s| s.fml3.as_ref());

        let still_advancing = match (fml3, client_state.forge_fml3_session_mut()) {
            (Some(snap), Some(session)) => emit_next_fml3_step(&mut batch, session, snap),
            _ => false,
        };

        if !still_advancing {
            // Snapshot exhausted — fall through to vanilla
            // Configuration sequence on the next inbound packet
            // (typically the client's brand message or the
            // AcknowledgeConfiguration that already exists).
            client_state.finish_forge_fml3_replay();
        }
        Ok(batch)
    }
}

/// Pushes the next FML3 snapshot step (if any) to the client as a
/// clientbound Configuration plugin message. Returns `true` if a step
/// was queued, `false` once the snapshot is exhausted.
fn emit_next_fml3_step(
    batch: &mut Batch<PacketRegistry>,
    session: &mut Fml3ReplaySession,
    snapshot: &crate::forge::snapshot::Fml3Snapshot,
) -> bool {
    let Some(step) = session.take_next_step(snapshot) else {
        return false;
    };
    let channel = parse_channel_identifier(&step.channel);
    // The configuration plugin message's `data` field is encoded as
    // `LengthPaddedVec<i8>` — convert the raw bytes verbatim.
    let data = step
        .payload
        .iter()
        .map(|&b| b.cast_signed())
        .collect::<Vec<i8>>();
    let packet = ConfigurationClientBoundPluginMessagePacket::raw(channel, data);
    batch.queue(move || PacketRegistry::ConfigurationClientBoundPluginMessage(packet));
    true
}

/// True for `fml:*` and `neoforge:*` channels — the two namespaces
/// Forge / `NeoForge` use for their FML3 handshake exchange.
fn is_forge_handshake_channel(channel: &str) -> bool {
    channel.starts_with("fml:") || channel.starts_with("neoforge:")
}

/// Parses an `namespace:value` channel string into an [`Identifier`]
/// using `Identifier::new_unchecked` to bypass strict grammar checks
/// (Forge channels occasionally contain characters the strict
/// constructor rejects, and we are forwarding bytes captured from a
/// known-good upstream).
fn parse_channel_identifier(channel: &str) -> Identifier {
    if let Some((ns, val)) = channel.split_once(':') {
        Identifier::new_unchecked(ns, val)
    } else {
        Identifier::new_unchecked("fml", "handshake")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_forge_handshake_channels() {
        assert!(is_forge_handshake_channel("fml:handshake"));
        assert!(is_forge_handshake_channel("fml:loginwrapper"));
        assert!(is_forge_handshake_channel("neoforge:handshake"));
        assert!(!is_forge_handshake_channel("minecraft:brand"));
        assert!(!is_forge_handshake_channel("velocity:player_info"));
    }

    #[test]
    fn parse_channel_identifier_round_trips_known_channels() {
        assert_eq!(
            parse_channel_identifier("fml:handshake").to_string(),
            "fml:handshake"
        );
        assert_eq!(
            parse_channel_identifier("neoforge:handshake").to_string(),
            "neoforge:handshake"
        );
    }

    #[test]
    fn parse_channel_identifier_falls_back_for_malformed() {
        let id = parse_channel_identifier("malformed_no_colon");
        // Falls back to a placeholder rather than panicking.
        assert!(id.to_string().contains(':'));
    }
}
