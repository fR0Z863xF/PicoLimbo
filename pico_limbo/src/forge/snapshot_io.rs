//! Persistence layer for [`Snapshot`].
//!
//! Two operations are provided:
//!
//! * [`load_snapshot`] — reads a snapshot from disk, transparently handling
//!   the file-not-found and unsupported-version cases.
//! * [`save_snapshot`] — writes a snapshot atomically (write-to-temp +
//!   rename) so a crash mid-write never leaves a half-baked file that the
//!   limbo would later refuse to start with.
//!
//! Both functions operate on `std::path::Path` directly and are
//! synchronous: snapshots are tiny (KB-range) and only touched at startup
//! / via the `/forge refresh` command, so adding async I/O here would just
//! be pageantry.

use crate::forge::snapshot::{Snapshot, SnapshotError};
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can happen during snapshot persistence.
#[derive(Debug, Error)]
pub enum SnapshotIoError {
    #[error("snapshot I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("snapshot JSON parse failed at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Outcome of [`load_snapshot`].
#[derive(Debug)]
pub enum LoadOutcome {
    /// File existed and parsed cleanly.
    Loaded(Box<Snapshot>),
    /// File did not exist at the requested path. Callers typically respond
    /// by triggering a fresh recording.
    Missing,
}

/// Attempts to read a snapshot from `path`. Returns
/// [`LoadOutcome::Missing`] when the file does not exist (a common steady
/// state — first startup) rather than erroring out.
///
/// Any other I/O or parsing failure is surfaced. Schema-version
/// mismatches are flagged via [`SnapshotError::UnsupportedVersion`].
pub fn load_snapshot(path: impl AsRef<Path>) -> Result<LoadOutcome, SnapshotIoError> {
    let path = path.as_ref();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LoadOutcome::Missing),
        Err(source) => {
            return Err(SnapshotIoError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|source| SnapshotIoError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    snapshot.check_version()?;
    Ok(LoadOutcome::Loaded(Box::new(snapshot)))
}

/// Atomically persists `snapshot` to `path`.
///
/// Writes to a sibling `<name>.tmp` file first and renames over the
/// destination so that readers always see either the previous valid
/// content or the new valid content — never a torn partial file. The
/// parent directory is created on demand.
pub fn save_snapshot(
    path: impl AsRef<Path>,
    snapshot: &Snapshot,
) -> Result<(), SnapshotIoError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| SnapshotIoError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let json = serde_json::to_vec_pretty(snapshot).map_err(|source| SnapshotIoError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let tmp = tmp_path_for(path);
    std::fs::write(&tmp, &json).map_err(|source| SnapshotIoError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| SnapshotIoError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Produces a temp-file path that lives next to `path` (so the final
/// `rename` is atomic on the same filesystem) and is obviously transient.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let new_name = match tmp.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(".tmp");
            owned
        }
        // Should never happen — `path` is supposed to point at a file.
        // Fall back to a generic name in the same directory.
        None => std::ffi::OsString::from("snapshot.tmp"),
    };
    tmp.set_file_name(new_name);
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::snapshot::{Fml2Snapshot, Fml2Step, Snapshot};

    fn unique_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("picolimbo_forge_{nonce}_{name}.json"));
        p
    }

    fn populated_snapshot() -> Snapshot {
        let mut s = Snapshot::new("upstream:25565");
        s.fml2 = Some(Fml2Snapshot {
            steps: vec![Fml2Step {
                channel: "fml:loginwrapper".into(),
                payload: vec![10, 20, 30, 40],
            }],
        });
        s
    }

    #[test]
    fn load_missing_returns_missing_outcome() {
        let path = unique_path("missing");
        // Intentionally do not create the file.
        match load_snapshot(&path).unwrap() {
            LoadOutcome::Missing => {}
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn save_then_load_round_trip() {
        let path = unique_path("round_trip");
        let snap = populated_snapshot();
        save_snapshot(&path, &snap).unwrap();
        let loaded = match load_snapshot(&path).unwrap() {
            LoadOutcome::Loaded(s) => *s,
            LoadOutcome::Missing => panic!("snapshot should exist"),
        };
        assert_eq!(loaded, snap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_is_pretty_printed() {
        let path = unique_path("pretty");
        let snap = populated_snapshot();
        save_snapshot(&path, &snap).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // serde_json::to_vec_pretty emits indented JSON — ensure the file
        // contains at least one newline so it is human-diffable.
        assert!(raw.contains('\n'), "snapshot file should be pretty-printed");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_corrupt_json() {
        let path = unique_path("corrupt");
        std::fs::write(&path, b"this is not json").unwrap();
        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotIoError::Parse { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_wrong_version() {
        let path = unique_path("wrong_version");
        let mut snap = populated_snapshot();
        snap.version = Snapshot::CURRENT_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&snap).unwrap()).unwrap();
        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(
            err,
            SnapshotIoError::Snapshot(SnapshotError::UnsupportedVersion { .. })
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_overwrites_existing_file_atomically() {
        let path = unique_path("overwrite");
        // First write.
        save_snapshot(&path, &populated_snapshot()).unwrap();
        // Now write a different one — the on-disk version must reflect the
        // *new* content with no temp file left behind.
        let mut newer = populated_snapshot();
        newer.upstream_addr = "different:25565".into();
        save_snapshot(&path, &newer).unwrap();
        let loaded = match load_snapshot(&path).unwrap() {
            LoadOutcome::Loaded(s) => *s,
            LoadOutcome::Missing => unreachable!(),
        };
        assert_eq!(loaded.upstream_addr, "different:25565");
        // No leftover temp file.
        let tmp = tmp_path_for(&path);
        assert!(!tmp.exists(), "temp file should have been renamed");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tmp_path_lives_next_to_target() {
        let p = PathBuf::from("/var/lib/picolimbo/snap.json");
        let tmp = tmp_path_for(&p);
        assert_eq!(tmp.parent(), p.parent());
        assert_eq!(tmp.file_name().unwrap(), "snap.json.tmp");
    }
}
