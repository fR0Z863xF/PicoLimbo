use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the Forge / NeoForge protocol bridge.
///
/// PicoLimbo can pretend to be a Forge server by *replaying* a snapshot of
/// the Login/Configuration-phase handshake recorded against an upstream
/// Forge bootstrap server, and by *passing through* the Status response's
/// `forgeData` field with a short-lived cache.
///
/// The whole feature is gated behind [`ForgeConfig::enabled`]; when disabled,
/// PicoLimbo behaves exactly like a vanilla limbo (the cost is one `match`
/// per Handshake packet).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    /// Master switch. When `false`, every other field is ignored and the
    /// limbo behaves identically to a non-Forge build.
    pub enabled: bool,

    /// Address of the upstream Forge/NeoForge bootstrap server used to:
    ///   * record the Login/Configuration handshake snapshot on startup
    ///     (when [`Self::record_on_start`] is `true`); and
    ///   * fetch the live `forgeData` field returned by the upstream's
    ///     status response.
    ///
    /// Expected form: `host:port`. The bootstrap server **must** run with
    /// `online-mode=false` and without any forwarding plugin (BungeeCord /
    /// Velocity), otherwise the recorder will be rejected and the snapshot
    /// will never be written.
    pub upstream: String,

    /// Path to the on-disk snapshot file. The file is created on first
    /// successful recording and re-read on subsequent restarts. Defaults to
    /// `./forge_snapshot.json` (human-readable JSON for debuggability).
    pub snapshot_path: PathBuf,

    /// If `true`, the limbo will attempt to (re)record the snapshot from
    /// [`Self::upstream`] every time it starts. If `false`, it will *only*
    /// read the existing file (useful when running offline or when the
    /// bootstrap server is intentionally not co-located).
    pub record_on_start: bool,

    /// TTL of the in-memory cache for the upstream's `forgeData` Status
    /// payload, in seconds. Pings that arrive within this window reuse the
    /// cached value; pings outside trigger a refresh.
    pub status_cache_ttl_secs: u64,

    /// Hard timeout, in milliseconds, for a single upstream Status request.
    /// On timeout PicoLimbo falls back to the snapshot's cached
    /// `forgeData` (if any) and serves the response anyway.
    pub status_request_timeout_ms: u64,

    /// Hard timeout, in milliseconds, for the entire startup-time recording
    /// session. If the upstream has not driven the handshake to LoginSuccess
    /// (FML2) / FinishConfiguration (FML3) by then, recording is aborted and
    /// the snapshot is *not* written.
    pub record_timeout_ms: u64,

    /// Username used by the recorder when impersonating a client against
    /// the upstream Forge server. Choose something obviously synthetic so
    /// operators can spot it in logs.
    pub recorder_username: String,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            upstream: "127.0.0.1:25566".into(),
            snapshot_path: PathBuf::from("forge_snapshot.json"),
            record_on_start: true,
            status_cache_ttl_secs: 60,
            status_request_timeout_ms: 3_000,
            record_timeout_ms: 10_000,
            // Must be ≤16 characters — Minecraft username limit.
            // `_picolimbo` is recognisably synthetic in logs.
            recorder_username: "_picolimbo".into(),
        }
    }
}

impl ForgeConfig {
    /// Convenience accessor used by hot paths to short-circuit Forge
    /// detection when the feature is fully off.
    #[allow(dead_code)] // Used by handshake/login handlers wired up in Steps 6-9.
    pub const fn is_active(&self) -> bool {
        self.enabled
    }

    /// Returns the configured TTL as a [`std::time::Duration`].
    #[allow(dead_code)] // Used by status_proxy added in Step 6.
    pub const fn status_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.status_cache_ttl_secs)
    }

    /// Returns the configured Status request timeout as a [`std::time::Duration`].
    #[allow(dead_code)] // Used by status_proxy added in Step 6.
    pub const fn status_request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.status_request_timeout_ms)
    }

    /// Returns the configured recording session timeout as a
    /// [`std::time::Duration`].
    #[allow(dead_code)] // Used by recorder added in Step 5.
    pub const fn record_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.record_timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_feature_off() {
        let cfg = ForgeConfig::default();
        assert!(!cfg.is_active());
        assert_eq!(cfg.upstream, "127.0.0.1:25566");
        assert_eq!(cfg.snapshot_path, PathBuf::from("forge_snapshot.json"));
        assert!(cfg.record_on_start);
    }

    #[test]
    fn durations_match_units() {
        let cfg = ForgeConfig {
            status_cache_ttl_secs: 120,
            status_request_timeout_ms: 1_500,
            record_timeout_ms: 20_000,
            ..ForgeConfig::default()
        };
        assert_eq!(cfg.status_cache_ttl().as_secs(), 120);
        assert_eq!(cfg.status_request_timeout().as_millis(), 1_500);
        assert_eq!(cfg.record_timeout().as_millis(), 20_000);
    }

    #[test]
    fn deserialises_partial_toml_with_defaults() {
        let toml = r#"
            enabled = true
            upstream = "10.0.0.5:25599"
        "#;
        let cfg: ForgeConfig = toml::from_str(toml).unwrap();
        assert!(cfg.is_active());
        assert_eq!(cfg.upstream, "10.0.0.5:25599");
        // Remaining fields fall back to default values.
        assert_eq!(cfg.status_cache_ttl_secs, 60);
        assert_eq!(cfg.recorder_username, "_picolimbo");
    }

    #[test]
    fn deny_unknown_fields() {
        let toml = r#"
            enabled = true
            unknown_field = "boom"
        "#;
        let parsed: Result<ForgeConfig, _> = toml::from_str(toml);
        assert!(parsed.is_err(), "deny_unknown_fields should reject typos");
    }

    #[test]
    fn round_trip_through_toml() {
        let cfg = ForgeConfig {
            enabled: true,
            upstream: "forge.internal:25565".into(),
            snapshot_path: PathBuf::from("/var/lib/picolimbo/forge.json"),
            record_on_start: false,
            status_cache_ttl_secs: 30,
            status_request_timeout_ms: 2_000,
            record_timeout_ms: 8_000,
            recorder_username: "MyRecorder".into(),
        };
        let serialised = toml::to_string(&cfg).unwrap();
        let parsed: ForgeConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(parsed.enabled, cfg.enabled);
        assert_eq!(parsed.upstream, cfg.upstream);
        assert_eq!(parsed.snapshot_path, cfg.snapshot_path);
        assert_eq!(parsed.record_on_start, cfg.record_on_start);
        assert_eq!(parsed.status_cache_ttl_secs, cfg.status_cache_ttl_secs);
        assert_eq!(parsed.recorder_username, cfg.recorder_username);
    }
}
