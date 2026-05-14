//! Thin async client that drives Minecraft connections **outwards** —
//! i.e. PicoLimbo connecting to another Minecraft server (the upstream
//! Forge / NeoForge bootstrap).
//!
//! Existing PicoLimbo code is server-side: it accepts a [`TcpStream`] and
//! reads serverbound packets / writes clientbound packets. The Forge
//! bridge needs to do the opposite: connect to a server, send serverbound
//! packets ourselves, and read clientbound packets back. We build that on
//! top of the same [`PacketStream`] wire codec so framing,
//! compression and timeouts are shared with the rest of the codebase.
//!
//! The client is *protocol-low-level*: it knows how to send a Handshake
//! and a LoginStart, how to read a [`RawPacket`], and how to drive
//! compression. It does **not** know about Forge — that lives in
//! `recorder.rs` / `status_proxy.rs` which build on top.

use minecraft_protocol::prelude::{BinaryReaderError, BinaryWriter, BinaryWriterError, VarInt};
use net::packet_stream::{PacketStream, PacketStreamError};
use net::raw_packet::RawPacket;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::error::Elapsed;
use tokio::time::timeout;

/// Stable Minecraft packet identifiers used by the outbound client.
///
/// These IDs have not changed since 1.13 for the directions and states
/// we care about (Handshake/Status/Login). We hard-code them here rather
/// than pulling them out of `PacketRegistry` because:
///
/// 1. `PacketRegistry::decode_packet` is hard-wired to dispatch
///    *serverbound* packets — exactly the opposite of what we need here.
/// 2. The IDs are part of the wire protocol contract; treating them as
///    constants is more honest about that contract.
pub mod packet_ids {
    // Serverbound (we send these).
    pub const SB_HANDSHAKE: u8 = 0x00;
    pub const SB_STATUS_REQUEST: u8 = 0x00;
    pub const SB_LOGIN_START: u8 = 0x00;
    pub const SB_LOGIN_PLUGIN_RESPONSE: u8 = 0x02;
    pub const SB_LOGIN_ACKNOWLEDGED: u8 = 0x03;
    pub const SB_CONFIG_PLUGIN_MESSAGE: u8 = 0x02;
    pub const SB_CONFIG_FINISH_CONFIGURATION: u8 = 0x03;
    /// Serverbound `Acknowledge Known Packs` (0x07 since 1.20.5). The
    /// reply our recorder fires after the upstream sends a
    /// clientbound Select Known Packs (0x0E) telling us which datapack
    /// versions it has. We acknowledge with an empty list to keep the
    /// handshake moving.
    pub const SB_CONFIG_ACKNOWLEDGE_KNOWN_PACKS: u8 = 0x07;

    // Clientbound (we read these).
    pub const CB_STATUS_RESPONSE: u8 = 0x00;
    pub const CB_LOGIN_DISCONNECT: u8 = 0x00;
    pub const CB_LOGIN_ENCRYPTION_REQUEST: u8 = 0x01;
    pub const CB_LOGIN_SUCCESS: u8 = 0x02;
    pub const CB_LOGIN_SET_COMPRESSION: u8 = 0x03;
    pub const CB_LOGIN_PLUGIN_REQUEST: u8 = 0x04;
    pub const CB_CONFIG_PLUGIN_MESSAGE: u8 = 0x01;
    pub const CB_CONFIG_FINISH_CONFIGURATION: u8 = 0x03;
    /// `Select Known Packs` (0x0E in 1.20.5+, clientbound). The server
    /// asks which datapack versions the client has installed so it
    /// can omit redundant registry-data entries. NeoForge ≥1.20.5
    /// blocks the rest of the configuration phase until the client
    /// acks this — see [`SB_CONFIG_ACKNOWLEDGE_KNOWN_PACKS`].
    pub const CB_CONFIG_SELECT_KNOWN_PACKS: u8 = 0x0E;
}

/// Intent value encoded in the second-stage `next_state` field of the
/// outbound Handshake packet. The numbers match the Minecraft protocol
/// (`1 = Status`, `2 = Login`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeIntent {
    Status,
    Login,
}

impl HandshakeIntent {
    pub const fn wire_value(self) -> i32 {
        match self {
            Self::Status => 1,
            Self::Login => 2,
        }
    }
}

/// Errors raised when running the outbound MC protocol client.
#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("connect to {addr} timed out after {timeout:?}")]
    ConnectTimeout { addr: String, timeout: Duration },

    #[error("operation timed out after {timeout:?}")]
    OperationTimeout { timeout: Duration },

    #[error("connect to {addr} failed: {source}")]
    Connect {
        addr: String,
        #[source]
        source: io::Error,
    },

    #[error("upstream returned unexpected packet (id={packet_id:#x}): {reason}")]
    UnexpectedPacket { packet_id: u8, reason: &'static str },

    #[error("upstream sent a disconnect during login: {0}")]
    LoginDisconnect(String),

    #[error("upstream requires online-mode authentication; recorder cannot complete it")]
    OnlineModeRequired,

    #[error(transparent)]
    Stream(#[from] PacketStreamError),

    #[error(transparent)]
    Encode(#[from] BinaryWriterError),

    #[error(transparent)]
    Decode(#[from] BinaryReaderError),

    #[error("malformed packet: {0}")]
    Malformed(String),
}

impl From<Elapsed> for UpstreamError {
    fn from(_: Elapsed) -> Self {
        Self::OperationTimeout {
            timeout: Duration::from_secs(0),
        }
    }
}

/// Outbound Minecraft client.
///
/// One instance corresponds to one TCP connection to an upstream server.
/// Construct with [`UpstreamClient::connect`], drive the protocol with
/// the typed helpers, then `drop` to close the socket.
pub struct UpstreamClient {
    stream: PacketStream<TcpStream>,
    peer_addr: String,
}

impl UpstreamClient {
    /// Opens a fresh TCP connection to the specified `addr`, wraps it in
    /// a [`PacketStream`] and returns a ready-to-use client.
    ///
    /// `addr` may be any string that `TcpStream::connect` accepts
    /// (`host:port`, `1.2.3.4:25565`, …). The whole connect operation
    /// is bounded by `connect_timeout`.
    pub async fn connect(
        addr: &str,
        connect_timeout: Duration,
    ) -> Result<Self, UpstreamError> {
        let socket = timeout(connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| UpstreamError::ConnectTimeout {
                addr: addr.to_string(),
                timeout: connect_timeout,
            })?
            .map_err(|source| UpstreamError::Connect {
                addr: addr.to_string(),
                source,
            })?;

        // Disable Nagle so handshake bursts go on the wire immediately.
        let _ = socket.set_nodelay(true);

        Ok(Self {
            stream: PacketStream::new(socket),
            peer_addr: addr.to_string(),
        })
    }

    /// Returns the address string passed to [`Self::connect`]; useful
    /// for log lines and error messages.
    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }

    /// Returns the underlying [`SocketAddr`] of the upstream as reported
    /// by the OS, if available.
    #[allow(dead_code)]
    pub fn raw_peer_addr(&self) -> Option<SocketAddr> {
        // PacketStream exposes `get_stream(&mut self)`; for the read-only
        // accessor we can't lazily mutate self, so we use a best-effort
        // approach via the constructed peer_addr string.
        self.peer_addr.parse().ok()
    }

    /// Sends the Handshake (serverbound 0x00) packet that initiates every
    /// Minecraft connection.
    ///
    /// `protocol_version` is the wire protocol number (e.g. `763` for
    /// 1.20.1, `769` for 1.21.4). `hostname` is sent verbatim, which is
    /// the right hook point to add the Forge `\0FML2\0` / `\0FML3\0`
    /// marker.
    pub async fn send_handshake(
        &mut self,
        protocol_version: i32,
        hostname: &str,
        port: u16,
        intent: HandshakeIntent,
    ) -> Result<(), UpstreamError> {
        let mut writer = BinaryWriter::new();
        writer.write(&packet_ids::SB_HANDSHAKE)?;
        writer.write(&VarInt::new(protocol_version))?;
        write_var_int_prefixed_string(&mut writer, hostname)?;
        writer.write(&port)?;
        writer.write(&VarInt::new(intent.wire_value()))?;

        let raw = RawPacket::new(writer.into_inner()).map_err(|_| {
            UpstreamError::Malformed("empty handshake packet".into())
        })?;
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends the Status Request (serverbound 0x00, empty body).
    pub async fn send_status_request(&mut self) -> Result<(), UpstreamError> {
        let raw = RawPacket::from_bytes(packet_ids::SB_STATUS_REQUEST, &[]);
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends a Login Start packet in the **1.20.2+** layout
    /// (`name: String`, `uuid: UUID(16 bytes)`).
    ///
    /// This is the format every Forge ≥ 1.20.1 and every NeoForge
    /// release accepts, which covers our entire FML2 (recorded with a
    /// `protocol_version` ≤ 763) **and** FML3 target range. For pre-1.19
    /// servers the recorder is currently not supported — see the design
    /// doc.
    pub async fn send_login_start(
        &mut self,
        protocol_version: i32,
        username: &str,
        uuid: uuid::Uuid,
    ) -> Result<(), UpstreamError> {
        let mut writer = BinaryWriter::new();
        writer.write(&packet_ids::SB_LOGIN_START)?;
        write_var_int_prefixed_string(&mut writer, username)?;

        // Layout selection based on the protocol version we declared
        // during the Handshake.
        //
        //   protocol >= 764 (1.20.2+)        →   name + uuid
        //   761  ≤ protocol < 764 (1.19.3+)  →   name + Optional<uuid>  (Some)
        //   759  ≤ protocol < 761 (1.19.0-2) →   name + Optional<SigData> (None) + Optional<uuid> (Some)
        //   protocol < 759 (≤ 1.18.2)         →   name (only, no uuid)
        //
        // For our recorder we always declare a modern version (≥ 763) in
        // `send_handshake`, so the most defensive policy that still
        // serves NeoForge 1.21 and Forge 1.20.x is to emit the
        // 1.20.2+ layout (mandatory UUID, 16 BE bytes).
        if protocol_version >= 764 {
            writer.write_bytes(uuid.as_bytes())?;
        } else if protocol_version >= 761 {
            // Optional<uuid>: present-flag byte + 16 bytes.
            writer.write(&1u8)?;
            writer.write_bytes(uuid.as_bytes())?;
        } else if protocol_version >= 759 {
            // Optional<sig_data> absent + Optional<uuid> present.
            writer.write(&0u8)?;
            writer.write(&1u8)?;
            writer.write_bytes(uuid.as_bytes())?;
        }
        // else: pre-1.19, just name; ignore the UUID.

        let raw = RawPacket::new(writer.into_inner()).map_err(|_| {
            UpstreamError::Malformed("empty login_start packet".into())
        })?;
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends a serverbound Login Plugin Response (id `0x02`), replying
    /// to a clientbound Login Plugin Request the upstream pushed.
    ///
    /// `data` is the raw payload to send back (commonly empty, or a
    /// short canned blob for FML mod-list negotiation). Pass `None` for
    /// data to send the "not present" form, which most Forge channels
    /// treat as a soft rejection.
    pub async fn send_login_plugin_response(
        &mut self,
        message_id: i32,
        data: Option<&[u8]>,
    ) -> Result<(), UpstreamError> {
        let mut writer = BinaryWriter::new();
        writer.write(&packet_ids::SB_LOGIN_PLUGIN_RESPONSE)?;
        writer.write(&VarInt::new(message_id))?;
        match data {
            Some(bytes) => {
                writer.write(&1u8)?;
                writer.write_bytes(bytes)?;
            }
            None => {
                writer.write(&0u8)?;
            }
        }
        let raw = RawPacket::new(writer.into_inner()).map_err(|_| {
            UpstreamError::Malformed("empty login_plugin_response".into())
        })?;
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends Login Acknowledged (1.20.2+, serverbound 0x03), the empty
    /// packet that transitions a client from Login to Configuration
    /// state.
    pub async fn send_login_acknowledged(&mut self) -> Result<(), UpstreamError> {
        let raw = RawPacket::from_bytes(packet_ids::SB_LOGIN_ACKNOWLEDGED, &[]);
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends a serverbound Configuration Plugin Message (id `0x02`)
    /// carrying `channel` and `data` — the FML3 reply primitive.
    pub async fn send_config_plugin_message(
        &mut self,
        channel: &str,
        data: &[u8],
    ) -> Result<(), UpstreamError> {
        let mut writer = BinaryWriter::new();
        writer.write(&packet_ids::SB_CONFIG_PLUGIN_MESSAGE)?;
        write_var_int_prefixed_string(&mut writer, channel)?;
        writer.write_bytes(data)?;
        let raw = RawPacket::new(writer.into_inner()).map_err(|_| {
            UpstreamError::Malformed("empty config_plugin_message".into())
        })?;
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends Acknowledge Finish Configuration (serverbound 0x03), the
    /// empty packet that closes out the Configuration state and switches
    /// the connection to Play.
    #[allow(dead_code)] // Used by recorder once FML3 handshake completes.
    pub async fn send_config_finish_configuration(&mut self) -> Result<(), UpstreamError> {
        let raw = RawPacket::from_bytes(packet_ids::SB_CONFIG_FINISH_CONFIGURATION, &[]);
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Sends `Acknowledge Known Packs` (serverbound 0x07) with an
    /// empty list — the canonical "I don't have any of the datapacks
    /// you advertised, send me the full registry" answer. NeoForge
    /// ≥1.20.5 requires this before it will continue the
    /// Configuration phase handshake.
    pub async fn send_config_acknowledge_known_packs_empty(
        &mut self,
    ) -> Result<(), UpstreamError> {
        // Body is a single VarInt(0) — zero known packs.
        let raw = RawPacket::from_bytes(
            packet_ids::SB_CONFIG_ACKNOWLEDGE_KNOWN_PACKS,
            &[0u8],
        );
        self.stream.write_packet(raw).await?;
        Ok(())
    }

    /// Reads exactly one [`RawPacket`] from the upstream, optionally
    /// bounded by `read_timeout`.
    pub async fn read_packet(
        &mut self,
        read_timeout: Duration,
    ) -> Result<RawPacket, UpstreamError> {
        let result = timeout(read_timeout, self.stream.read_packet()).await;
        match result {
            Ok(Ok(packet)) => Ok(packet),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(UpstreamError::OperationTimeout {
                timeout: read_timeout,
            }),
        }
    }

    /// Enables zlib compression on subsequent reads/writes. Called when
    /// the upstream pushes a Login Set Compression (0x03) packet.
    pub fn set_compression(&mut self, threshold: usize, level: u32) {
        self.stream.set_compression(threshold, level);
    }

    /// Shuts the underlying socket down. Safe to call multiple times.
    #[allow(dead_code)]
    pub async fn close(mut self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.stream.get_stream().shutdown().await;
    }
}

/// Serialises a string in the canonical Minecraft VarInt-length-prefixed
/// format and appends it to `writer`.
fn write_var_int_prefixed_string(
    writer: &mut BinaryWriter,
    value: &str,
) -> Result<(), BinaryWriterError> {
    let bytes = value.as_bytes();
    writer.write(&VarInt::new(i32::try_from(bytes.len())?))?;
    writer.write_bytes(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_intent_wire_values() {
        assert_eq!(HandshakeIntent::Status.wire_value(), 1);
        assert_eq!(HandshakeIntent::Login.wire_value(), 2);
    }

    #[test]
    fn write_var_int_prefixed_string_encodes_length_then_bytes() {
        let mut writer = BinaryWriter::new();
        write_var_int_prefixed_string(&mut writer, "hello").unwrap();
        // "hello" is 5 bytes → VarInt(5) = 0x05, then UTF-8 of "hello".
        assert_eq!(writer.into_inner(), b"\x05hello");
    }

    #[test]
    fn write_var_int_prefixed_string_handles_empty() {
        let mut writer = BinaryWriter::new();
        write_var_int_prefixed_string(&mut writer, "").unwrap();
        assert_eq!(writer.into_inner(), b"\x00");
    }

    #[test]
    fn write_var_int_prefixed_string_handles_long_strings() {
        // 200 bytes — forces a 2-byte VarInt prefix (200 = 0xC8 0x01).
        let s = "a".repeat(200);
        let mut writer = BinaryWriter::new();
        write_var_int_prefixed_string(&mut writer, &s).unwrap();
        let bytes = writer.into_inner();
        assert_eq!(&bytes[..2], &[0xC8, 0x01]);
        assert_eq!(bytes.len(), 2 + 200);
    }

    #[test]
    fn upstream_error_from_elapsed_collapses_to_operation_timeout() {
        // `Elapsed` has no public constructor in stable tokio; fake one
        // via a 1ns sleep that always trips.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let elapsed: Result<(), Elapsed> = rt.block_on(async {
            tokio::time::timeout(Duration::from_nanos(1), async {
                tokio::time::sleep(Duration::from_secs(1)).await;
            })
            .await
        });
        assert!(elapsed.is_err());
        let err: UpstreamError = elapsed.unwrap_err().into();
        assert!(matches!(err, UpstreamError::OperationTimeout { .. }));
    }

    #[test]
    fn handshake_intent_round_trip_values() {
        for intent in [HandshakeIntent::Status, HandshakeIntent::Login] {
            let wire = intent.wire_value();
            assert!(matches!(wire, 1 | 2));
        }
    }

    /// **Network-dependent test**, only runs when a real upstream MC
    /// server is reachable at the address below. Set the env var
    /// `PICOLIMBO_TEST_UPSTREAM` to override.
    ///
    /// Marked `#[ignore]` so default `cargo test` runs stay offline.
    /// Invoke manually with:
    /// ```text
    /// cargo test -p pico_limbo --lib forge::upstream_client -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires a live Minecraft server at PICOLIMBO_TEST_UPSTREAM"]
    async fn live_status_ping_against_real_server() {
        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());

        let mut client = UpstreamClient::connect(&addr, Duration::from_secs(5))
            .await
            .expect("connect");

        // 769 is the wire protocol number for 1.21.4 — using a recent
        // value here means a server running pretty much any 1.21.x build
        // will still serve us a status response (the version negotiation
        // happens after Login, not Status).
        client
            .send_handshake(769, "localhost", 25565, HandshakeIntent::Status)
            .await
            .expect("handshake");
        client
            .send_status_request()
            .await
            .expect("status request");

        let raw = client
            .read_packet(Duration::from_secs(5))
            .await
            .expect("read status response");

        assert_eq!(raw.packet_id(), Some(packet_ids::CB_STATUS_RESPONSE));
        let mut reader =
            minecraft_protocol::prelude::BinaryReader::new(raw.data());
        let json: minecraft_protocol::prelude::VarIntPrefixedString =
            reader.read().expect("read status string");
        let body = json.into_inner();
        eprintln!("== upstream status JSON ==\n{body}");

        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("parse status JSON");
        let has_forge_data = parsed.get("forgeData").is_some();
        eprintln!("forgeData present? {has_forge_data}");
        // We don't hard-assert on `forgeData` here — vanilla servers
        // legitimately omit it. The point of this test is to prove the
        // end-to-end wire path works against a live server.
    }
}
