//! On-disk representation of a recorded Forge handshake.
//!
//! A [`Snapshot`] captures everything `PicoLimbo` needs at runtime to convince
//! a Forge / `NeoForge` client that it is talking to a real Forge server:
//!
//! * the ordered list of *server-bound-to-client* plugin messages exchanged
//!   during the FML2 (Login phase) and/or FML3 (Configuration phase)
//!   handshake (see [`Fml2Snapshot`] / [`Fml3Snapshot`]);
//! * a verbatim copy of the upstream server's `forgeData` Status field
//!   (see [`Snapshot::status_forge_data`]) — used as fallback when the
//!   live cache is empty or the upstream is unreachable.
//!
//! Snapshots are persisted as JSON for two reasons: it allows operators to
//! diff them between recordings, and it side-steps the need to introduce a
//! new binary-serialisation crate (e.g. `bincode`/`postcard`) into the
//! workspace.
//!
//! # Versioning
//!
//! Each persisted snapshot carries a [`Snapshot::version`] integer. When the
//! on-disk schema changes in a backwards-incompatible way, bump
//! [`Snapshot::CURRENT_VERSION`] and add a migration in
//! [`crate::forge::snapshot_io`].

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Errors raised when manipulating snapshots.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// A snapshot was loaded from disk but its `version` field does not
    /// match what this build of `PicoLimbo` understands. The caller should
    /// either run a migration or rerecord.
    #[error("unsupported snapshot version: found {found}, expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },

    /// A snapshot was loaded but lacks the section requested by the
    /// caller (e.g. an FML3 client connected but the snapshot only has FML2
    /// data). Callers typically translate this into a friendly Login
    /// disconnect.
    #[error("snapshot is missing the {0:?} handshake section")]
    MissingSection(SnapshotSection),
}

/// Identifies a sub-section of a [`Snapshot`] that may or may not be
/// populated. Used by [`SnapshotError::MissingSection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `StatusForgeData` is consumed by the status_proxy added in Step 6.
pub enum SnapshotSection {
    Fml2,
    Fml3,
    StatusForgeData,
}

/// A single server→client step of the FML2 (Login phase) handshake.
///
/// The `payload` is the raw `data` field of the corresponding clientbound
/// `Login Plugin Request` packet (i.e. *not* including the `message_id`
/// `VarInt` — that is generated fresh at replay time, see
/// [`crate::forge::snapshot::Fml2Snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fml2Step {
    /// Channel identifier such as `"fml:loginwrapper"` or
    /// `"fml:handshake"`. Stored as a plain string because Forge channel
    /// names can contain characters outside the strict
    /// `pico_identifier::Identifier` grammar (`NeoForge` in particular uses
    /// custom namespaces).
    pub channel: String,
    /// Opaque payload bytes. Replayed verbatim — `PicoLimbo` never inspects
    /// the contents.
    #[serde(with = "base64_bytes")]
    pub payload: Vec<u8>,
}

/// All the FML2 handshake steps recorded in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fml2Snapshot {
    pub steps: Vec<Fml2Step>,
}

impl Fml2Snapshot {
    /// Returns the step at `idx`, or `None` if the snapshot is exhausted.
    pub fn get(&self, idx: usize) -> Option<&Fml2Step> {
        self.steps.get(idx)
    }

    /// Number of recorded steps.
    pub const fn len(&self) -> usize {
        self.steps.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// A single server→client step of the FML3 (Configuration phase) handshake.
///
/// Unlike FML2, configuration plugin messages do not carry a `message_id`,
/// so the replay state machine simply walks this `Vec` in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fml3Step {
    pub channel: String,
    #[serde(with = "base64_bytes")]
    pub payload: Vec<u8>,
}

/// All the FML3 handshake steps recorded in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fml3Snapshot {
    pub steps: Vec<Fml3Step>,
}

impl Fml3Snapshot {
    // The accessors below are public surface for the FML3 replay state
    // machine added in Step 9; #[allow(dead_code)] keeps Step 3 warning-free.
    #[allow(dead_code)]
    pub fn get(&self, idx: usize) -> Option<&Fml3Step> {
        self.steps.get(idx)
    }

    #[allow(dead_code)]
    pub const fn len(&self) -> usize {
        self.steps.len()
    }

    #[allow(dead_code)]
    pub const fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The on-disk container persisted by the recorder and consumed by the
/// replay state machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// On-disk schema version. See [`Self::CURRENT_VERSION`].
    pub version: u32,

    /// Unix timestamp (seconds) of when the snapshot was captured. Used by
    /// operators to gauge staleness; not load-bearing in any code path.
    pub captured_at_unix: u64,

    /// Address (`host:port`) of the upstream Forge server the recording
    /// was made against. Stored verbatim from
    /// [`crate::configuration::forge::ForgeConfig::upstream`] so that an
    /// operator looking at a snapshot file knows exactly what produced it.
    pub upstream_addr: String,

    /// FML2 (Login-phase) handshake. `None` when the upstream is FML3-only
    /// or when recording for FML2 failed.
    pub fml2: Option<Fml2Snapshot>,

    /// FML3 (Configuration-phase) handshake. `None` when the upstream is
    /// FML2-only or when recording for FML3 failed.
    pub fml3: Option<Fml3Snapshot>,

    /// Verbatim copy of the upstream's Status `forgeData` field, captured
    /// at the same time as the handshake. Used as a fallback when the live
    /// cache is empty.
    pub status_forge_data: Option<serde_json::Value>,
}

impl Snapshot {
    /// Bump this whenever the persisted layout changes incompatibly.
    pub const CURRENT_VERSION: u32 = 1;

    /// Returns an empty snapshot tagged with the current schema version
    /// and the supplied upstream address. Useful when the recorder wants
    /// to start populating fields incrementally.
    pub fn new(upstream_addr: impl Into<String>) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            captured_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            upstream_addr: upstream_addr.into(),
            fml2: None,
            fml3: None,
            status_forge_data: None,
        }
    }

    /// Returns the FML2 snapshot or [`SnapshotError::MissingSection`].
    pub fn require_fml2(&self) -> Result<&Fml2Snapshot, SnapshotError> {
        self.fml2
            .as_ref()
            .ok_or(SnapshotError::MissingSection(SnapshotSection::Fml2))
    }

    /// Returns the FML3 snapshot or [`SnapshotError::MissingSection`].
    pub fn require_fml3(&self) -> Result<&Fml3Snapshot, SnapshotError> {
        self.fml3
            .as_ref()
            .ok_or(SnapshotError::MissingSection(SnapshotSection::Fml3))
    }

    /// Returns `Ok(())` if the schema version matches what this build of
    /// `PicoLimbo` understands; otherwise an [`SnapshotError::UnsupportedVersion`].
    pub const fn check_version(&self) -> Result<(), SnapshotError> {
        if self.version == Self::CURRENT_VERSION {
            Ok(())
        } else {
            Err(SnapshotError::UnsupportedVersion {
                found: self.version,
                expected: Self::CURRENT_VERSION,
            })
        }
    }
}

/// Helper module that serialises `Vec<u8>` as base64 instead of a JSON
/// array of integers. This keeps the on-disk file ~30% smaller and far
/// easier to skim by hand while staying within `serde_json` (no extra
/// crate required — `base64` is already in workspace deps).
mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        let encoded = STANDARD.encode(bytes);
        ser.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s: String = String::deserialize(de)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            version: Snapshot::CURRENT_VERSION,
            captured_at_unix: 1_700_000_000,
            upstream_addr: "127.0.0.1:25566".into(),
            fml2: Some(Fml2Snapshot {
                steps: vec![
                    Fml2Step {
                        channel: "fml:loginwrapper".into(),
                        payload: vec![0x01, 0x02, 0x03, 0xff],
                    },
                    Fml2Step {
                        channel: "fml:handshake".into(),
                        payload: vec![],
                    },
                ],
            }),
            fml3: Some(Fml3Snapshot {
                steps: vec![Fml3Step {
                    channel: "fml:handshake".into(),
                    payload: vec![0xde, 0xad, 0xbe, 0xef],
                }],
            }),
            status_forge_data: Some(serde_json::json!({
                "fmlNetworkVersion": 3,
                "channels": [],
                "mods": [],
            })),
        }
    }

    #[test]
    fn new_snapshot_uses_current_version() {
        let snap = Snapshot::new("h:1");
        assert_eq!(snap.version, Snapshot::CURRENT_VERSION);
        assert_eq!(snap.upstream_addr, "h:1");
        assert!(snap.fml2.is_none());
        assert!(snap.fml3.is_none());
        assert!(snap.status_forge_data.is_none());
    }

    #[test]
    fn check_version_rejects_mismatch() {
        let mut snap = Snapshot::new("h");
        snap.version = Snapshot::CURRENT_VERSION + 1;
        assert!(matches!(
            snap.check_version(),
            Err(SnapshotError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn require_fml_returns_missing_when_unset() {
        let snap = Snapshot::new("h");
        assert!(matches!(
            snap.require_fml2(),
            Err(SnapshotError::MissingSection(SnapshotSection::Fml2))
        ));
        assert!(matches!(
            snap.require_fml3(),
            Err(SnapshotError::MissingSection(SnapshotSection::Fml3))
        ));
    }

    #[test]
    fn json_round_trip_preserves_payload_bytes() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snap);
    }

    #[test]
    fn payload_is_base64_encoded_in_json() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        // 0x01,0x02,0x03,0xff -> "AQID/w=="
        assert!(json.contains("\"payload\":\"AQID/w==\""));
        // 0xde,0xad,0xbe,0xef -> "3q2+7w=="
        assert!(json.contains("\"payload\":\"3q2+7w==\""));
    }

    #[test]
    fn snapshot_section_accessors() {
        let snap = sample_snapshot();
        assert_eq!(snap.require_fml2().unwrap().len(), 2);
        assert_eq!(snap.require_fml3().unwrap().len(), 1);
        assert_eq!(
            snap.require_fml2().unwrap().get(0).unwrap().channel,
            "fml:loginwrapper"
        );
    }

    #[test]
    fn empty_step_lists_are_handled() {
        let empty = Fml2Snapshot::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.get(0).is_none());
    }
}
