use minecraft_protocol::prelude::*;

/// Serverbound counterpart of
/// [`ConfigurationClientBoundPluginMessagePacket`]: a
/// `Configuration → Server` plugin message carrying a
/// channel identifier and an opaque payload.
///
/// Used by the Forge / NeoForge `fml:handshake` flow on Minecraft
/// 1.20.2+, where the FML handshake lives in the Configuration phase.
/// PicoLimbo's replay state machine consumes these packets to know
/// when the client has acknowledged the previous outbound step.
#[derive(PacketIn)]
pub struct ConfigurationServerBoundPluginMessagePacket {
    pub channel: Identifier,
    /// Remaining payload bytes after the channel identifier. The
    /// `Vec<u8>` `PacketIn` reader greedily consumes the rest of the
    /// packet, which matches the wire format (no length prefix — the
    /// outer packet length implicitly bounds the payload).
    pub data: Vec<u8>,
}
