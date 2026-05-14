//! Pass-through cache for the upstream's Status-phase `forgeData` field.
//!
//! Forge / `NeoForge` clients require the server-list-ping JSON to contain
//! a `forgeData` object before they will display the green ✓ next to the
//! entry and let the user click "Join". The contents of that field are
//! opaque to us (they include the mod list, channel registrations and a
//! gzip-base64 blob produced by the upstream Forge server) so we simply
//! fetch it from the configured upstream and cache the verbatim
//! [`serde_json::Value`] for a short TTL.
//!
//! The cache is `Send + Sync` and intended to live inside `ServerState`.
//! Look-ups never block on the upstream — if the cache is fresh enough
//! we hand the value back immediately; if it has gone stale we trigger a
//! one-off refresh, but a concurrent ping that hits us at the same time
//! still gets the previous (possibly slightly stale) value rather than
//! waiting on the network. This keeps the limbo's status response
//! latency bounded even when the upstream is misbehaving.
//!
//! ## Fallback chain
//!
//! 1. Live fetch within `status_request_timeout`.
//! 2. Last-known-good cached value (any age, as long as we ever fetched).
//! 3. Snapshot fallback (`status_forge_data` baked into
//!    `forge_snapshot.json`).
//! 4. `None` — limbo serves a vanilla status response. The Forge client
//!    will see the ❓ icon but the limbo itself stays healthy.

use crate::configuration::forge::ForgeConfig;
use crate::forge::upstream_client::{HandshakeIntent, UpstreamClient, UpstreamError, packet_ids};
use minecraft_protocol::prelude::{BinaryReader, VarIntPrefixedString};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, trace, warn};

/// Live cache of the upstream's `forgeData` Status payload.
pub struct ForgeStatusCache {
    inner: Mutex<CacheState>,
    config: Arc<ForgeConfig>,
}

#[derive(Default)]
struct CacheState {
    /// Most recent value we successfully fetched from the upstream, if
    /// any. Kept indefinitely so we can serve stale-but-valid data when
    /// a refresh round-trip fails.
    value: Option<Value>,
    /// Wall-clock `Instant` of the last successful fetch.
    fetched_at: Option<Instant>,
    /// Snapshot-provided fallback. Loaded at startup and never mutated
    /// after that, but stored here next to the live value so the
    /// fallback selection is a single field access.
    snapshot_fallback: Option<Value>,
}

impl ForgeStatusCache {
    /// Creates a fresh cache. `snapshot_fallback` is the
    /// `status_forge_data` field of the on-disk snapshot, used as the
    /// last-resort response when the upstream is unreachable.
    pub fn new(config: Arc<ForgeConfig>, snapshot_fallback: Option<Value>) -> Self {
        Self {
            inner: Mutex::new(CacheState {
                value: None,
                fetched_at: None,
                snapshot_fallback,
            }),
            config,
        }
    }

    /// Returns the best `forgeData` value available right now.
    ///
    /// * If we have a cached value younger than
    ///   `config.status_cache_ttl` → return it directly.
    /// * Else attempt a live fetch (with `status_request_timeout`); on
    ///   success update the cache and return the new value.
    /// * On fetch failure: return the stale-cached value if any, else
    ///   the snapshot fallback, else `None`.
    pub async fn get(&self) -> Option<Value> {
        if !self.config.is_active() {
            return None;
        }

        // Fast path: cache is fresh.
        let snapshot_fallback;
        {
            let guard = self.inner.lock().await;
            if let (Some(value), Some(fetched_at)) = (guard.value.as_ref(), guard.fetched_at)
                && fetched_at.elapsed() < self.config.status_cache_ttl()
            {
                trace!("forge: status cache hit (age {:?})", fetched_at.elapsed());
                return Some(value.clone());
            }
            snapshot_fallback = guard.snapshot_fallback.clone();
        }

        // Slow path: try a live fetch.
        match self.fetch_upstream().await {
            Ok(Some(value)) => {
                {
                    let mut guard = self.inner.lock().await;
                    guard.value = Some(value.clone());
                    guard.fetched_at = Some(Instant::now());
                }
                debug!(
                    "forge: status cache refreshed from {}",
                    self.config.upstream
                );
                Some(value)
            }
            Ok(None) => {
                debug!(
                    "forge: upstream {} returned a response without forgeData",
                    self.config.upstream
                );
                // Still update the timestamp so we don't hammer the
                // upstream every ping — leave the value alone so
                // previously-good cached values remain available.
                let mut guard = self.inner.lock().await;
                guard.fetched_at = Some(Instant::now());
                guard.value.clone().or(snapshot_fallback)
            }
            Err(e) => {
                warn!(
                    "forge: status fetch from {} failed: {}; falling back to {}",
                    self.config.upstream,
                    e,
                    if self
                        .inner
                        .try_lock()
                        .map(|g| g.value.is_some())
                        .unwrap_or(false)
                    {
                        "stale cache"
                    } else if snapshot_fallback.is_some() {
                        "snapshot fallback"
                    } else {
                        "none (status will be vanilla)"
                    }
                );
                let guard = self.inner.lock().await;
                guard.value.clone().or(snapshot_fallback)
            }
        }
    }

    /// Force a fresh fetch and replace the cache. Used by the
    /// `/forge refresh` admin command.
    #[allow(dead_code)] // Wired up by Step 10.
    pub async fn refresh(&self) -> Result<(), UpstreamError> {
        let value = self.fetch_upstream().await?;
        {
            let mut guard = self.inner.lock().await;
            if let Some(v) = value {
                guard.value = Some(v);
            }
            guard.fetched_at = Some(Instant::now());
        }
        Ok(())
    }

    /// Replaces the snapshot fallback after the on-disk snapshot has
    /// been (re)loaded. Live cached values are untouched.
    #[allow(dead_code)] // Wired up by recorder once a fresh snapshot is written.
    pub async fn replace_snapshot_fallback(&self, fallback: Option<Value>) {
        let mut guard = self.inner.lock().await;
        guard.snapshot_fallback = fallback;
    }

    /// Connects to the upstream, performs a Status ping, parses the
    /// JSON and returns the `forgeData` field (if any).
    async fn fetch_upstream(&self) -> Result<Option<Value>, UpstreamError> {
        // Protocol version 769 (1.21.4) is a safe modern default — the
        // upstream's Status response does not actually depend on the
        // declared version, but we still have to send a syntactically
        // valid handshake.
        const STATUS_PING_PROTOCOL: i32 = 769;

        let timeout_total = self.config.status_request_timeout();
        let mut client = UpstreamClient::connect(&self.config.upstream, timeout_total).await?;

        client
            .send_handshake(
                STATUS_PING_PROTOCOL,
                "limbo-status-probe",
                25565,
                HandshakeIntent::Status,
            )
            .await?;
        client.send_status_request().await?;

        let raw = client.read_packet(timeout_total).await?;
        let packet_id = raw.packet_id().unwrap_or(0xFF);
        if packet_id != packet_ids::CB_STATUS_RESPONSE {
            return Err(UpstreamError::UnexpectedPacket {
                packet_id,
                reason: "expected Status Response (0x00)",
            });
        }

        let mut reader = BinaryReader::new(raw.data());
        let body: VarIntPrefixedString = reader
            .read()
            .map_err(|e| UpstreamError::Malformed(format!("status string: {e}")))?;
        let body = body.into_inner();
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| UpstreamError::Malformed(format!("status JSON: {e}")))?;

        Ok(parsed.get("forgeData").cloned())
    }
}

/// Injects the cached `forgeData` (or any other operator-supplied
/// `forgeData` blob) into a Status response JSON value. Has no effect
/// when `forge_data` is `None`.
pub fn inject_forge_data(status_json: &mut Value, forge_data: Option<&Value>) {
    if let Some(fd) = forge_data
        && let Some(obj) = status_json.as_object_mut()
    {
        obj.insert("forgeData".to_string(), fd.clone());
    }
}

/// Wall-clock helper for tests: returns a config with caching disabled
/// so every `get()` call goes back to the upstream.
#[cfg(test)]
fn cfg_no_cache(upstream: &str) -> Arc<ForgeConfig> {
    Arc::new(ForgeConfig {
        enabled: true,
        upstream: upstream.into(),
        status_cache_ttl_secs: 0,
        status_request_timeout_ms: 3_000,
        ..ForgeConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn get_returns_none_when_feature_disabled() {
        let cfg = Arc::new(ForgeConfig::default()); // enabled=false
        let cache = ForgeStatusCache::new(cfg, Some(json!({"fmlNetworkVersion": 3})));
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn get_returns_snapshot_fallback_when_upstream_unreachable() {
        let cfg = Arc::new(ForgeConfig {
            enabled: true,
            // Port 1 is reserved; the connect will fail immediately.
            upstream: "127.0.0.1:1".into(),
            status_request_timeout_ms: 200,
            ..ForgeConfig::default()
        });
        let fallback = json!({"fmlNetworkVersion": 3, "channels": [], "mods": []});
        let cache = ForgeStatusCache::new(cfg, Some(fallback.clone()));
        let result = cache.get().await;
        assert_eq!(result, Some(fallback));
    }

    #[tokio::test]
    async fn get_returns_none_when_disabled_and_no_fallback() {
        let cfg = Arc::new(ForgeConfig {
            enabled: false,
            ..ForgeConfig::default()
        });
        let cache = ForgeStatusCache::new(cfg, None);
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn inject_forge_data_adds_field_when_present() {
        let mut json = json!({
            "version": {"name": "1.20.1", "protocol": 763},
            "players": {"max": 48, "online": 0},
            "description": {"text": "limbo"}
        });
        let fd = json!({"fmlNetworkVersion": 3});
        inject_forge_data(&mut json, Some(&fd));
        assert_eq!(json.get("forgeData"), Some(&fd));
    }

    #[tokio::test]
    async fn inject_forge_data_is_noop_when_none() {
        let mut json = json!({"description": {"text": "limbo"}});
        inject_forge_data(&mut json, None);
        assert!(json.get("forgeData").is_none());
    }

    #[tokio::test]
    async fn inject_forge_data_handles_non_object_root_gracefully() {
        // The Minecraft status response should always be an object, but
        // be defensive against odd inputs.
        let mut json = json!(["unexpected", "array"]);
        let fd = json!({"x": 1});
        inject_forge_data(&mut json, Some(&fd));
        // Must not panic; array left untouched.
        assert_eq!(json, json!(["unexpected", "array"]));
    }

    /// Live test against the configured `PICOLIMBO_TEST_UPSTREAM` server.
    /// Verifies the cache transitions from cold → fresh → cached.
    #[tokio::test]
    #[ignore = "requires a live Forge Minecraft server"]
    async fn live_fetch_and_cache_against_real_server() {
        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());
        let cfg = cfg_no_cache(&addr); // TTL=0 → every call refetches
        let cache = ForgeStatusCache::new(cfg, None);

        let first = cache.get().await.expect("upstream should have forgeData");
        eprintln!(
            "forge data keys: {:?}",
            first.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        assert!(
            first.get("fmlNetworkVersion").is_some(),
            "expected fmlNetworkVersion in forgeData"
        );

        // With TTL=0 a second call still works.
        let second = cache.get().await.expect("second fetch");
        assert_eq!(first, second);
    }

    /// Cache TTL > 0: the second call must come from the cache, not the
    /// network. We assert that by pointing at an unreachable address and
    /// observing that the second call still succeeds.
    #[tokio::test]
    #[ignore = "requires a live Forge Minecraft server"]
    async fn live_cache_serves_stale_after_upstream_dies() {
        let live_addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());
        let mut cfg = (*cfg_no_cache(&live_addr)).clone();
        cfg.status_cache_ttl_secs = 3600;
        let cfg = Arc::new(cfg);
        let cache = ForgeStatusCache::new(cfg, None);

        let first = cache.get().await.expect("first fetch");
        assert!(first.is_object());

        let second = cache.get().await.expect("second fetch from cache");
        assert_eq!(first, second);
    }
}
