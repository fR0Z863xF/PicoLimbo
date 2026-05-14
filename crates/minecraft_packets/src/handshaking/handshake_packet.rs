use minecraft_protocol::prelude::*;

#[derive(Clone, PacketIn)]
pub struct HandshakePacket {
    pub protocol: VarInt,
    pub hostname: String,
    pub port: u16,
    /// 1: Status, 2: Login, 3: Transfer
    pub next_state: VarInt,
}

impl HandshakePacket {
    pub fn localhost(protocol: i32, next_state: i32) -> Self {
        Self {
            hostname: String::from("localhost"),
            port: 25565,
            protocol: protocol.into(),
            next_state: next_state.into(),
        }
    }

    /// Convenience accessor that runs Forge marker detection on the inner
    /// hostname. Returns the detected [`ForgeKind`] and a cleaned hostname
    /// suitable for forwarding (BungeeCord/Velocity) detection that does not
    /// know about the Forge marker.
    pub fn detect_forge(&self) -> (ForgeKind, String) {
        ForgeKind::detect(&self.hostname)
    }
}

/// Identifies which Forge handshake dialect a connecting client expects.
///
/// Forge / NeoForge clients advertise themselves to a server by appending a
/// NUL-separated marker to the `hostname` field of the very first Handshake
/// packet:
///
/// | Marker           | Mc versions             | Variant                  |
/// |------------------|-------------------------|--------------------------|
/// | `\0FML\0`        | 1.7  – 1.12.2           | [`ForgeKind::Fml1`]      |
/// | `\0FML2\0`       | 1.13 – 1.20.1           | [`ForgeKind::Fml2`]      |
/// | `\0FML3\0`       | 1.20.2+ Forge/NeoForge  | [`ForgeKind::Fml3`]      |
///
/// PicoLimbo only supports the Login-/Configuration-phase handshakes
/// (`Fml2` and `Fml3`) — see [`ForgeKind::is_supported`] for the precise
/// matrix. `Fml1` is currently detected but treated as unsupported, since its
/// handshake happens *after* entering the Play state via the legacy
/// `FML|HS` plugin channel and is out of scope for the first iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForgeKind {
    /// Vanilla / non-Forge client — no marker found.
    #[default]
    None,
    /// `\0FML\0` marker (legacy FML, Mc 1.7-1.12).
    Fml1,
    /// `\0FML2\0` marker (Login Plugin Request handshake, Mc 1.13-1.20.1).
    Fml2,
    /// `\0FML3\0` marker (Configuration Plugin Message handshake, Mc 1.20.2+).
    Fml3,
}

impl ForgeKind {
    /// Inspects a raw handshake `hostname` field, looks for a Forge marker
    /// (`\0FML\0`, `\0FML2\0`, `\0FML3\0`) and returns the detected variant
    /// together with the hostname with that marker stripped out so that
    /// downstream consumers (e.g. BungeeCord/Velocity legacy-forwarding
    /// detection that also relies on `\0`-separated payloads) can keep
    /// working untouched.
    ///
    /// When multiple markers happen to appear in the same string the longest
    /// one wins — this matches what real Forge launchers emit (they only ever
    /// emit *one* marker) but guards against pathological inputs.
    ///
    /// The function is `O(n)` and allocates only when a marker is actually
    /// found, which keeps the vanilla-client hot path free of cost.
    pub fn detect(hostname: &str) -> (Self, String) {
        // Order matters: try the longest markers first so an `\0FML3\0`
        // string is not accidentally classified as `\0FML\0`.
        const MARKERS: &[(&str, ForgeKind)] = &[
            ("\0FML3\0", ForgeKind::Fml3),
            ("\0FML2\0", ForgeKind::Fml2),
            ("\0FML\0", ForgeKind::Fml1),
        ];

        for (marker, kind) in MARKERS {
            if let Some(idx) = hostname.find(marker) {
                let mut cleaned = String::with_capacity(hostname.len() - marker.len());
                cleaned.push_str(&hostname[..idx]);
                cleaned.push_str(&hostname[idx + marker.len()..]);
                return (*kind, cleaned);
            }
        }

        (Self::None, hostname.to_string())
    }

    /// Returns `true` when PicoLimbo can honour this Forge dialect end-to-end
    /// (i.e. server-list ✓ + Login/Configuration handshake replay + entry
    /// into the limbo world). `Fml1` is intentionally excluded — its
    /// handshake lives in the Play state and is not in scope.
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::None | Self::Fml2 | Self::Fml3)
    }

    /// Returns `true` when this client expects PicoLimbo to actively drive a
    /// FML handshake (i.e. drain a snapshot of plugin messages). Vanilla and
    /// `Fml1` connections do not need any extra packets.
    pub const fn requires_handshake(self) -> bool {
        matches!(self, Self::Fml2 | Self::Fml3)
    }

    /// Human-readable name suitable for log lines and kick messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "Vanilla",
            Self::Fml1 => "FML (1.7-1.12)",
            Self::Fml2 => "FML2 (1.13-1.20.1)",
            Self::Fml3 => "FML3 (1.20.2+)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ForgeKind;
    use crate::handshaking::handshake_packet::HandshakePacket;
    use minecraft_protocol::prelude::{BinaryReader, DecodePacket, ProtocolVersion, VarInt};

    #[test]
    fn detect_forge_returns_none_for_vanilla() {
        let (kind, cleaned) = ForgeKind::detect("mc.example.com");
        assert_eq!(kind, ForgeKind::None);
        assert_eq!(cleaned, "mc.example.com");
    }

    #[test]
    fn detect_forge_recognises_fml1() {
        let (kind, cleaned) = ForgeKind::detect("mc.example.com\0FML\0");
        assert_eq!(kind, ForgeKind::Fml1);
        assert_eq!(cleaned, "mc.example.com");
    }

    #[test]
    fn detect_forge_recognises_fml2() {
        let (kind, cleaned) = ForgeKind::detect("mc.example.com\0FML2\0");
        assert_eq!(kind, ForgeKind::Fml2);
        assert_eq!(cleaned, "mc.example.com");
    }

    #[test]
    fn detect_forge_recognises_fml3() {
        let (kind, cleaned) = ForgeKind::detect("mc.example.com\0FML3\0");
        assert_eq!(kind, ForgeKind::Fml3);
        assert_eq!(cleaned, "mc.example.com");
    }

    #[test]
    fn detect_forge_strips_marker_even_when_followed_by_bungeecord_payload() {
        // BungeeCord stacks its own `\0`-separated payload (real-ip, uuid, ...)
        // after the original hostname; Forge clients prepend their marker
        // before that payload. Make sure stripping the Forge marker preserves
        // the rest so legacy forwarding still parses correctly.
        let raw = "mc.example.com\0FML2\0\0127.0.0.1\0deadbeef";
        let (kind, cleaned) = ForgeKind::detect(raw);
        assert_eq!(kind, ForgeKind::Fml2);
        assert_eq!(cleaned, "mc.example.com\0127.0.0.1\0deadbeef");
    }

    #[test]
    fn detect_forge_prefers_longer_marker_when_ambiguous() {
        // If for some reason both markers appear (should not happen in real
        // traffic but guards against future weirdness), the longer one wins.
        let (kind, _) = ForgeKind::detect("host\0FML3\0\0FML\0");
        assert_eq!(kind, ForgeKind::Fml3);
    }

    #[test]
    fn forge_kind_supported_flags() {
        assert!(ForgeKind::None.is_supported());
        assert!(ForgeKind::Fml2.is_supported());
        assert!(ForgeKind::Fml3.is_supported());
        assert!(!ForgeKind::Fml1.is_supported());

        assert!(!ForgeKind::None.requires_handshake());
        assert!(!ForgeKind::Fml1.requires_handshake());
        assert!(ForgeKind::Fml2.requires_handshake());
        assert!(ForgeKind::Fml3.requires_handshake());
    }

    #[test]
    fn handshake_packet_helper_detects_forge() {
        let pkt = HandshakePacket {
            protocol: VarInt::new(763),
            hostname: "play.mc.com\0FML3\0".to_string(),
            port: 25565,
            next_state: VarInt::new(2),
        };
        let (kind, cleaned) = pkt.detect_forge();
        assert_eq!(kind, ForgeKind::Fml3);
        assert_eq!(cleaned, "play.mc.com");
    }

    #[test]
    fn test_handshake_packet_decode() {
        let handshake_snapshot = [
            129, 6, 9, 108, 111, 99, 97, 108, 104, 111, 115, 116, 99, 221, 1,
        ];
        let mut reader = BinaryReader::new(&handshake_snapshot);
        let protocol_version = ProtocolVersion::V1_21_4;
        let expected_protocol = VarInt::new(769);
        let expected_hostname = "localhost".to_string();
        let expected_port = 25565;
        let expected_next_state = VarInt::new(1);

        let packet = HandshakePacket::decode(&mut reader, protocol_version).unwrap();

        assert_eq!(expected_protocol, packet.protocol);
        assert_eq!(expected_hostname, packet.hostname);
        assert_eq!(expected_port, packet.port);
        assert_eq!(expected_next_state, packet.next_state);
    }
}
