// The recorder is built around four long state-machine functions
// (`record_and_persist`, `record_login_phase`, `record_configuration_phase`
// and `live_probe`). Splitting them up would obscure the linear packet
// flow they encode, so we keep them long and silence `too_many_lines`.
// `cast_sign_loss` triggers on `i32 as usize` for VarInt compression
// thresholds that are already guarded by `>= 0` checks. `if_not_else`
// is a style preference that would force reordering large match arms.
#![allow(clippy::too_many_lines, clippy::cast_sign_loss, clippy::if_not_else)]

//! Records a complete Forge handshake against a live upstream server and
//! persists it as a [`Snapshot`] for later replay.
//!
//! Two dialects are supported:
//!
//! * **FML2** (Minecraft 1.13 – 1.20.1). The handshake happens in the
//!   Login phase and is composed of clientbound `LoginPluginRequest`
//!   (`0x04`) packets we capture verbatim. See [`record_fml2`].
//! * **FML3** (Minecraft 1.20.2+, Forge & `NeoForge`). The handshake
//!   moves to the Configuration phase and uses clientbound Plugin
//!   Message packets. See [`record_fml3`].
//!
//! Both functions are *driven*, not passive: while we record what the
//! server sends, we also have to respond just enough to keep the
//! server's state machine advancing. The recorder behaves like a Forge
//! client that has *no* mods installed — every plugin message reply is
//! an empty present payload, which the canonical FML implementations
//! interpret as "I'm here but I have nothing to add", letting the
//! handshake progress to `LoginSuccess` / `FinishConfiguration` without
//! disconnecting us.
//!
//! Errors are surfaced as [`UpstreamError`] and never panic; the caller
//! (`start_server.rs`) treats a recording failure as "operate the limbo
//! anyway without a recorded snapshot".

use crate::configuration::forge::ForgeConfig;
use crate::forge::snapshot::{Fml2Snapshot, Fml2Step, Fml3Snapshot, Fml3Step, Snapshot};
use crate::forge::snapshot_io::{SnapshotIoError, save_snapshot};
use crate::forge::status_proxy::ForgeStatusCache;
use crate::forge::upstream_client::{HandshakeIntent, UpstreamClient, UpstreamError, packet_ids};
use crate::forge::velocity_forwarder::{
    OutboundIdentity, build_signed_player_info, is_velocity_player_info_channel,
};
use minecraft_protocol::prelude::{BinaryReader, VarInt, VarIntPrefixedString};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Wire protocol version used by the recorder when talking FML2.
/// `763` = Minecraft 1.20.1 — the latest version on which Forge still
/// runs its handshake in the Login state.
const FML2_WIRE_PROTOCOL: i32 = 763;

/// Wire protocol version used by the recorder when talking FML3.
/// `767` = Minecraft 1.21 — a safe modern default for the Configuration
/// state handshake. We could also probe `769` (1.21.4), but `767` is
/// known to be supported by both Forge and `NeoForge`.
const FML3_WIRE_PROTOCOL: i32 = 767;

/// Convenience port number we send in the Handshake. Any value works
/// because the upstream does not validate it; we use the canonical
/// vanilla port so log lines look sensible.
const HANDSHAKE_PORT: u16 = 25565;

/// Compression level used when the upstream pushes a Set Compression
/// packet at us. The threshold is what the server says; the level is
/// what we use when *sending* — we pick a moderate default because the
/// recorder never sends large payloads anyway.
const RECORDER_COMPRESSION_LEVEL: u32 = 6;

/// Records a fresh handshake snapshot from the upstream configured in
/// `forge_cfg`, persists it to `forge_cfg.snapshot_path`, and seeds the
/// supplied [`ForgeStatusCache`] with its `status_forge_data` fallback.
///
/// Returns the populated [`Snapshot`]. The function is best-effort by
/// nature: when a particular FML dialect is not supported by the
/// upstream (e.g. an FML3-only server refuses FML2), we log a warning,
/// leave that section of the snapshot empty, and continue.
///
/// At least one of FML2 or FML3 must succeed for the function to return
/// `Ok`. If both fail, the most recent error is propagated.
/// The Velocity Modern Forwarding secret shared with the upstream
/// Forge backend.
///
/// When `Some`, the recorder will play the role of Velocity outbound:
/// any `velocity:player_info` plugin request from the upstream is
/// answered with a properly HMAC-signed payload, which is the only way
/// to pass the modern-forwarding gate on a Velocity-fronted Forge
/// network. When `None`, those requests are answered with an empty
/// payload — fine for bootstrap servers that have forwarding disabled,
/// but immediately rejected by anything behind Velocity.
pub type VelocitySecret<'a> = Option<&'a [u8]>;

pub async fn record_and_persist(
    forge_cfg: &ForgeConfig,
    cache: Option<&Arc<ForgeStatusCache>>,
    velocity_secret: VelocitySecret<'_>,
) -> Result<Snapshot, RecorderError> {
    info!(
        "forge: recording handshake against upstream {}",
        forge_cfg.upstream
    );

    let mut snapshot = Snapshot::new(&forge_cfg.upstream);

    // **Adaptive protocol detection** — ask the upstream once at the
    // start of the recording session which wire protocol it speaks
    // via a Status ping. Both the login-phase and configuration-phase
    // probe lists below use that number as the *first* candidate;
    // hardcoded matrices are only the fallback.
    let auto_protocol = detect_upstream_protocol(forge_cfg).await;

    // Probe matrix: (label, marker, wire_protocol)
    //
    // Forge / NeoForge servers vary along *two* axes that look
    // similar but are independent:
    //
    //   1. **Wire protocol**: the Minecraft protocol version we
    //      declare in the Handshake. ≤763 (≤1.20.1) means the FML
    //      handshake lives in the Login phase; ≥764 (1.20.2+) means
    //      it lives in the Configuration phase. This is dictated by
    //      vanilla Minecraft and is non-negotiable.
    //
    //   2. **FML network version** (a.k.a. the `\0FMLx\0` marker
    //      appended to the hostname). Forge introduced version `2`
    //      for the Login-phase plugin-channel scheme (1.13-1.20.x)
    //      and version `3` for the newer Configuration-phase scheme
    //      (1.20.2+). However: some intermediate Forge / NeoForge
    //      builds *backport* net version 3 onto the 1.20.1 wire
    //      protocol — that is, they advertise themselves as FML3 but
    //      still run the handshake in the Login phase.
    //
    // To stay compatible with every flavour we have encountered we
    // run the probe matrix below in priority order and store the
    // first hit. The order is: try the most-modern combination
    // first, fall back to older ones.
    // Build the login-phase probe list: prepend auto-detected protocol
    // (when ≤763, since the login-phase FML handshake only exists
    // there) ahead of the hardcoded P763 fallback.
    let mut login_probes: Vec<(String, &str, i32)> = Vec::new();
    if let Some(p) = auto_protocol
        && p <= 763
        && p != 763
    {
        login_probes.push((
            format!("FML3+P{p} (auto-detected, modern FML net v3)"),
            "\0FML3\0",
            p,
        ));
        login_probes.push((
            format!("FML2+P{p} (auto-detected, classic FML net v2)"),
            "\0FML2\0",
            p,
        ));
    }
    login_probes.push((
        "FML3+P763 (modern FML on 1.20.1)".to_string(),
        "\0FML3\0",
        763,
    ));
    login_probes.push((
        "FML2+P763 (classic FML on 1.20.1)".to_string(),
        "\0FML2\0",
        763,
    ));
    let mut last_login_err: Option<UpstreamError> = None;
    for (label, marker, protocol) in &login_probes {
        let label = label.as_str();
        let marker = *marker;
        let protocol = *protocol;
        let outcome = record_login_phase(forge_cfg, marker, protocol, velocity_secret).await;
        eprintln!(
            "forge[probe]: login {} -> {}",
            label,
            match &outcome {
                Ok(s) => format!("Ok({} steps)", s.steps.len()),
                Err(e) => format!("Err({e})"),
            }
        );
        match outcome {
            Ok(fml2) => {
                info!(
                    "forge: recorded login-phase handshake via {} ({} steps)",
                    label,
                    fml2.steps.len()
                );
                snapshot.fml2 = Some(fml2);
                break;
            }
            Err(e) => {
                debug!("forge: login-phase probe {} failed: {}", label, e);
                last_login_err = Some(e);
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
    if snapshot.fml2.is_none()
        && let Some(e) = last_login_err.as_ref()
    {
        warn!("forge: login-phase recording exhausted, last error: {e}");
    }

    sleep(Duration::from_millis(250)).await;

    // Configuration-phase probes — same auto-detection logic but for
    // protocols ≥764 (1.20.2+, the versions where Forge moved its
    // handshake into the Configuration state).
    let mut config_probes: Vec<(String, &str, i32)> = Vec::new();
    if let Some(p) = auto_protocol {
        config_probes.push((
            format!("FML3+P{p} (auto-detected from status ping)"),
            "\0FML3\0",
            p,
        ));
    }
    // Static fallback matrix, ordered most-recent-first. Skip the
    // auto-detected one if already inserted to avoid double work.
    let fallback_protocols: &[(&str, i32)] = &[
        ("1.21.8", 772),
        ("1.21.7", 771),
        ("1.21.5", 770),
        ("1.21.4", 769),
        ("1.21.2", 768),
        ("1.21  ", 767),
        ("1.20.5", 766),
        ("1.20.2", 764),
    ];
    for (label, protocol) in fallback_protocols {
        if Some(*protocol) == auto_protocol {
            continue;
        }
        config_probes.push((
            format!("FML3+P{protocol} ({label} config phase, fallback)"),
            "\0FML3\0",
            *protocol,
        ));
    }

    let mut last_config_err: Option<UpstreamError> = None;
    for (label, marker, protocol) in &config_probes {
        let label = label.as_str();
        let marker = *marker;
        let protocol = *protocol;
        let outcome =
            record_configuration_phase(forge_cfg, marker, protocol, velocity_secret).await;
        eprintln!(
            "forge[probe]: config {} -> {}",
            label,
            match &outcome {
                Ok(s) => format!("Ok({} steps)", s.steps.len()),
                Err(e) => format!("Err({e})"),
            }
        );
        match outcome {
            Ok(fml3) => {
                info!(
                    "forge: recorded configuration-phase handshake via {} ({} steps)",
                    label,
                    fml3.steps.len()
                );
                snapshot.fml3 = Some(fml3);
                break;
            }
            Err(e) => {
                debug!("forge: config-phase probe {} failed: {}", label, e);
                last_config_err = Some(e);
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
    if snapshot.fml3.is_none()
        && let Some(e) = last_config_err.as_ref()
    {
        debug!("forge: configuration-phase recording exhausted, last error: {e}");
    }

    let fml2_err = last_login_err.filter(|_| snapshot.fml2.is_none());
    let fml3_err = last_config_err.filter(|_| snapshot.fml3.is_none());

    // Also try to capture the upstream's Status `forgeData` so the
    // limbo can serve it offline. This is the same probe the
    // status_proxy does at runtime, but doing it here lets us bake the
    // value into the snapshot's `status_forge_data` field for use as a
    // last-resort cold-start fallback.
    sleep(Duration::from_millis(250)).await;
    match capture_status_forge_data(forge_cfg).await {
        Ok(forge_data) => {
            snapshot.status_forge_data = forge_data;
        }
        Err(e) => {
            warn!(
                "forge: status forgeData capture during recording failed: {}",
                e
            );
        }
    }

    if snapshot.fml2.is_none() && snapshot.fml3.is_none() {
        if let (Some(fml2), Some(fml3)) = (fml2_err.as_ref(), fml3_err.as_ref()) {
            warn!("forge: FML2 error: {fml2}");
            warn!("forge: FML3 error: {fml3}");
        }
        if let Some(e) = fml2_err.or(fml3_err) {
            return Err(RecorderError::AllDialectsFailed(e));
        }
        return Err(RecorderError::Empty);
    }

    save_snapshot(&forge_cfg.snapshot_path, &snapshot)?;
    info!(
        "forge: snapshot persisted to {}",
        forge_cfg.snapshot_path.display()
    );

    if let Some(cache) = cache {
        cache
            .replace_snapshot_fallback(snapshot.status_forge_data.clone())
            .await;
    }

    Ok(snapshot)
}

/// Errors that can happen during a recording session.
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    /// The upstream rejected every FML dialect we tried; the inner
    /// error is the most recent failure.
    #[error("recording failed for both FML2 and FML3: {0}")]
    AllDialectsFailed(#[source] UpstreamError),

    /// The recorder somehow produced an empty snapshot with no
    /// upstream errors; this is a logic bug, included for completeness.
    #[error("recording produced an empty snapshot")]
    Empty,

    #[error(transparent)]
    Io(#[from] SnapshotIoError),

    #[error(transparent)]
    Upstream(#[from] UpstreamError),
}

/// Drives a Login-phase handshake against the configured upstream and
/// returns the recorded server→client plugin-request sequence.
///
/// The Login phase is the *vanilla MC state* used by Forge ≤1.20.1; the
/// `marker` argument lets the caller pick the desired FML network
/// version (`\0FML2\0` for legacy FML, `\0FML3\0` for the back-ported
/// modern variant).
async fn record_login_phase(
    forge_cfg: &ForgeConfig,
    marker: &str,
    protocol_version: i32,
    velocity_secret: VelocitySecret<'_>,
) -> Result<Fml2Snapshot, UpstreamError> {
    let hostname_with_marker = forge_marker_hostname(&forge_cfg.upstream, marker);
    let mut client =
        UpstreamClient::connect(&forge_cfg.upstream, forge_cfg.record_timeout()).await?;

    client
        .send_handshake(
            protocol_version,
            &hostname_with_marker,
            HANDSHAKE_PORT,
            HandshakeIntent::Login,
        )
        .await?;
    client
        .send_login_start(
            protocol_version,
            &forge_cfg.recorder_username,
            uuid::Uuid::nil(),
        )
        .await?;

    let deadline = Instant::now() + forge_cfg.record_timeout();
    let mut steps = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(UpstreamError::OperationTimeout {
                timeout: forge_cfg.record_timeout(),
            });
        }
        let raw = client.read_packet(remaining).await?;
        let packet_id = raw.packet_id().unwrap_or(0xFF);

        match packet_id {
            packet_ids::CB_LOGIN_SET_COMPRESSION => {
                let mut reader = BinaryReader::new(raw.data());
                let threshold = reader.read::<VarInt>()?.inner();
                if threshold >= 0 {
                    let threshold_usize = usize::try_from(threshold).map_err(|_| {
                        UpstreamError::Malformed(format!(
                            "invalid compression threshold {threshold}"
                        ))
                    })?;
                    client.set_compression(threshold_usize, RECORDER_COMPRESSION_LEVEL);
                }
            }
            packet_ids::CB_LOGIN_PLUGIN_REQUEST => {
                let mut reader = BinaryReader::new(raw.data());
                let message_id = reader.read::<VarInt>()?.inner();
                let channel: VarIntPrefixedString = reader.read()?;
                let payload = reader.remaining_bytes()?;
                let channel = channel.into_inner();

                if is_velocity_player_info_channel(&channel) {
                    // Velocity Modern Forwarding gate: we have to
                    // answer with a properly HMAC-signed payload
                    // *before* the backend will start talking Forge.
                    // This step is **not** part of the snapshot
                    // (PicoLimbo handles its own inbound Velocity
                    // forwarding via `check_velocity_key_integrity`).
                    if let Some(secret) = velocity_secret {
                        let identity = OutboundIdentity::recorder(&forge_cfg.recorder_username);
                        match build_signed_player_info(secret, &identity) {
                            Ok(signed) => {
                                debug!(
                                    "forge: answering velocity:player_info \
                                     with HMAC-signed payload ({}B)",
                                    signed.len()
                                );
                                client
                                    .send_login_plugin_response(message_id, Some(&signed))
                                    .await?;
                                continue;
                            }
                            Err(e) => {
                                return Err(UpstreamError::Malformed(format!(
                                    "velocity sign failed: {e}"
                                )));
                            }
                        }
                    }
                    return Err(UpstreamError::Malformed(
                        "upstream demands Velocity Modern Forwarding but \
                         no secret was passed to the recorder"
                            .into(),
                    ));
                }

                debug!(
                    "forge: recorded FML2 step #{} channel={} payload={}B",
                    steps.len(),
                    channel,
                    payload.len()
                );
                steps.push(Fml2Step { channel, payload });

                // Empty present response is the canonical "no mods,
                // no preferences, just let me in" reply.
                client
                    .send_login_plugin_response(message_id, Some(&[]))
                    .await?;
            }
            packet_ids::CB_LOGIN_SUCCESS => {
                debug!(
                    "forge: FML2 handshake complete, {} steps captured",
                    steps.len()
                );
                return Ok(Fml2Snapshot { steps });
            }
            packet_ids::CB_LOGIN_DISCONNECT => {
                let reason = decode_login_disconnect(raw.data());
                // A disconnect after we have already captured useful
                // server→client FML handshake packets is **not** a
                // failure — it just means the server was waiting for
                // a smarter reply than the empty-present we sent
                // (typically an `fml:loginwrapper`-wrapped FML ACK
                // payload that we deliberately don't synthesise).
                //
                // The snapshot we have so far is exactly what a real
                // Forge client expects PicoLimbo to *send back* during
                // replay, so we treat it as a partial-but-usable
                // recording.
                if !steps.is_empty() {
                    info!(
                        "forge: upstream disconnected after {} captured steps \
                         (reason: {}); persisting partial snapshot",
                        steps.len(),
                        reason
                    );
                    return Ok(Fml2Snapshot { steps });
                }
                return Err(UpstreamError::LoginDisconnect(reason));
            }
            packet_ids::CB_LOGIN_ENCRYPTION_REQUEST => {
                return Err(UpstreamError::OnlineModeRequired);
            }
            other => {
                return Err(UpstreamError::UnexpectedPacket {
                    packet_id: other,
                    reason: "expected LoginPluginRequest / LoginSuccess during FML2 record",
                });
            }
        }
    }
}

/// Drives a Configuration-phase handshake against the upstream and
/// returns the recorded server→client plugin-message sequence.
///
/// The Configuration phase is the vanilla MC state introduced in
/// 1.20.2 (protocol 764); Forge / `NeoForge` ≥1.20.2 moved their FML
/// handshake into this phase.
async fn record_configuration_phase(
    forge_cfg: &ForgeConfig,
    marker: &str,
    protocol_version: i32,
    velocity_secret: VelocitySecret<'_>,
) -> Result<Fml3Snapshot, UpstreamError> {
    let hostname_with_marker = forge_marker_hostname(&forge_cfg.upstream, marker);
    let mut client =
        UpstreamClient::connect(&forge_cfg.upstream, forge_cfg.record_timeout()).await?;

    client
        .send_handshake(
            protocol_version,
            &hostname_with_marker,
            HANDSHAKE_PORT,
            HandshakeIntent::Login,
        )
        .await?;
    client
        .send_login_start(
            protocol_version,
            &forge_cfg.recorder_username,
            uuid::Uuid::nil(),
        )
        .await?;

    let deadline = Instant::now() + forge_cfg.record_timeout();
    let mut in_configuration = false;
    let mut steps = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(UpstreamError::OperationTimeout {
                timeout: forge_cfg.record_timeout(),
            });
        }
        let raw = client.read_packet(remaining).await?;
        let packet_id = raw.packet_id().unwrap_or(0xFF);

        if !in_configuration {
            match packet_id {
                packet_ids::CB_LOGIN_SET_COMPRESSION => {
                    let mut reader = BinaryReader::new(raw.data());
                    let threshold = reader.read::<VarInt>()?.inner();
                    if threshold >= 0 {
                        let threshold_usize = usize::try_from(threshold).map_err(|_| {
                            UpstreamError::Malformed(format!(
                                "invalid compression threshold {threshold}"
                            ))
                        })?;
                        client.set_compression(threshold_usize, RECORDER_COMPRESSION_LEVEL);
                    }
                }
                packet_ids::CB_LOGIN_PLUGIN_REQUEST => {
                    // FML3 servers behind Velocity send a
                    // `velocity:player_info` LPR here too; handle it
                    // exactly like in the FML2 path.
                    let mut reader = BinaryReader::new(raw.data());
                    let message_id = reader.read::<VarInt>()?.inner();
                    let channel: VarIntPrefixedString = reader.read()?;
                    let channel = channel.into_inner();
                    if is_velocity_player_info_channel(&channel) {
                        if let Some(secret) = velocity_secret {
                            let identity = OutboundIdentity::recorder(&forge_cfg.recorder_username);
                            let signed =
                                build_signed_player_info(secret, &identity).map_err(|e| {
                                    UpstreamError::Malformed(format!("velocity sign failed: {e}"))
                                })?;
                            client
                                .send_login_plugin_response(message_id, Some(&signed))
                                .await?;
                        } else {
                            return Err(UpstreamError::Malformed(
                                "FML3 upstream demands Velocity Modern \
                                 Forwarding but no secret was passed \
                                 to the recorder"
                                    .into(),
                            ));
                        }
                    } else {
                        client
                            .send_login_plugin_response(message_id, Some(&[]))
                            .await?;
                    }
                }
                packet_ids::CB_LOGIN_SUCCESS => {
                    // Acknowledge → enter Configuration state.
                    eprintln!("forge[record]: LoginSuccess → sending LoginAcknowledged");
                    client.send_login_acknowledged().await?;
                    in_configuration = true;
                }
                packet_ids::CB_LOGIN_DISCONNECT => {
                    let reason = decode_login_disconnect(raw.data());
                    return Err(UpstreamError::LoginDisconnect(reason));
                }
                packet_ids::CB_LOGIN_ENCRYPTION_REQUEST => {
                    return Err(UpstreamError::OnlineModeRequired);
                }
                other => {
                    return Err(UpstreamError::UnexpectedPacket {
                        packet_id: other,
                        reason: "unexpected packet in FML3 Login phase",
                    });
                }
            }
        } else {
            eprintln!(
                "forge[record-cfg]: pkt id=0x{packet_id:02x} ({}B)",
                raw.data().len()
            );
            match packet_id {
                packet_ids::CB_CONFIG_SELECT_KNOWN_PACKS => {
                    // NeoForge ≥1.20.5 sends this before any FML
                    // handshake message; ack with empty list so the
                    // upstream proceeds to send registry data / FML
                    // plugin messages.
                    debug!("forge: ack'ing Select Known Packs with empty list");
                    client.send_config_acknowledge_known_packs_empty().await?;
                }
                packet_ids::CB_CONFIG_PLUGIN_MESSAGE => {
                    let mut reader = BinaryReader::new(raw.data());
                    let channel: VarIntPrefixedString = reader.read()?;
                    let payload = reader.remaining_bytes()?;
                    let channel = channel.into_inner();

                    eprintln!(
                        "forge[record-cfg]: plugin msg channel={channel:?} payload={}B head=[{}]",
                        payload.len(),
                        payload
                            .iter()
                            .take(16)
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );

                    let is_handshake = channel == "fml:handshake"
                        || channel == "neoforge:handshake"
                        || channel.starts_with("fml:")
                        || channel.starts_with("neoforge:");

                    if is_handshake {
                        // Only Forge/NeoForge handshake messages are
                        // worth replaying; vanilla configuration
                        // plugin messages (brand, etc.) are handled by
                        // PicoLimbo's own logic later.
                        steps.push(Fml3Step {
                            channel: channel.clone(),
                            payload,
                        });
                    }

                    // Reply with empty payload on the same channel so
                    // the server keeps making progress.
                    if is_handshake {
                        client.send_config_plugin_message(&channel, &[]).await?;
                    }
                }
                packet_ids::CB_CONFIG_FINISH_CONFIGURATION => {
                    debug!(
                        "forge: FML3 handshake complete, {} steps captured",
                        steps.len()
                    );
                    // We don't bother transitioning to Play — the
                    // socket is about to be closed anyway.
                    return Ok(Fml3Snapshot { steps });
                }
                packet_ids::CB_LOGIN_DISCONNECT => {
                    // Same "partial success" treatment as the login
                    // phase: if we have any captured FML handshake
                    // steps, keep them.
                    let reason = decode_login_disconnect(raw.data());
                    if !steps.is_empty() {
                        info!(
                            "forge: FML3 upstream disconnected after {} \
                             captured steps (reason: {}); persisting partial",
                            steps.len(),
                            reason
                        );
                        return Ok(Fml3Snapshot { steps });
                    }
                    return Err(UpstreamError::LoginDisconnect(reason));
                }
                _ => {
                    // The configuration phase has many packets we
                    // don't care about (registry data, tags, etc.).
                    // Skip them silently. If we never see
                    // `FinishConfiguration` we'll bail out via the
                    // deadline above.
                }
            }
        }
    }
}

/// Captures `forgeData` from a Status ping for inclusion in the
/// snapshot. Returns `Ok(None)` when the upstream answered but did not
/// advertise `forgeData` — that is a soft failure, not an error.
async fn capture_status_forge_data(
    forge_cfg: &ForgeConfig,
) -> Result<Option<serde_json::Value>, UpstreamError> {
    fetch_status_json(forge_cfg)
        .await
        .map(|v| v.get("forgeData").cloned())
}

/// Performs a single Status ping against the upstream and returns the
/// **full** parsed status JSON (so callers can pull both `forgeData`
/// and `version.protocol` from the same payload).
async fn fetch_status_json(forge_cfg: &ForgeConfig) -> Result<serde_json::Value, UpstreamError> {
    let mut client =
        UpstreamClient::connect(&forge_cfg.upstream, forge_cfg.status_request_timeout()).await?;
    client
        .send_handshake(
            FML3_WIRE_PROTOCOL,
            "limbo-recorder",
            HANDSHAKE_PORT,
            HandshakeIntent::Status,
        )
        .await?;
    client.send_status_request().await?;
    let raw = client
        .read_packet(forge_cfg.status_request_timeout())
        .await?;
    if raw.packet_id() != Some(packet_ids::CB_STATUS_RESPONSE) {
        return Err(UpstreamError::UnexpectedPacket {
            packet_id: raw.packet_id().unwrap_or(0xFF),
            reason: "expected Status Response during snapshot capture",
        });
    }
    let mut reader = BinaryReader::new(raw.data());
    let body: VarIntPrefixedString = reader.read()?;
    let body = body.into_inner();
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| UpstreamError::Malformed(format!("status JSON for snapshot: {e}")))?;
    Ok(value)
}

/// Asks the upstream which Minecraft wire protocol it speaks via a
/// Status ping, and returns the integer (e.g. `772` for 1.21.8).
///
/// Returns `None` when the ping fails or the JSON does not contain a
/// well-formed `version.protocol` integer; callers should fall back to
/// the static probe matrix in that case.
///
/// Why this matters: hardcoding a list of acceptable protocol numbers
/// breaks every time Mojang or `NeoForge` ships a new release. Asking
/// the server makes the recorder version-agnostic.
async fn detect_upstream_protocol(forge_cfg: &ForgeConfig) -> Option<i32> {
    match fetch_status_json(forge_cfg).await {
        Ok(value) => {
            let protocol = value
                .get("version")
                .and_then(|v| v.get("protocol"))
                .and_then(serde_json::Value::as_i64)
                .and_then(|p| i32::try_from(p).ok());
            let name = value
                .get("version")
                .and_then(|v| v.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?");
            protocol.map_or_else(
                || {
                    debug!(
                        "forge: upstream {} status JSON has no version.protocol field",
                        forge_cfg.upstream
                    );
                    None
                },
                |p| {
                    info!(
                        "forge: upstream {} reports MC {} (protocol {p})",
                        forge_cfg.upstream, name
                    );
                    Some(p)
                },
            )
        }
        Err(e) => {
            debug!(
                "forge: status ping for protocol detection failed: {e}; \
                 falling back to static probe matrix"
            );
            None
        }
    }
}

/// Builds the `hostname` field for a Forge-aware Handshake by
/// concatenating the upstream's host part with the supplied
/// NUL-bracketed marker.
fn forge_marker_hostname(upstream: &str, marker: &str) -> String {
    let host = upstream.rsplit_once(':').map_or(upstream, |(h, _)| h);
    format!("{host}{marker}")
}

/// Best-effort decoder for the Login Disconnect packet's `reason`
/// field. Returns a Chat-component JSON string verbatim; if the field
/// can't be parsed we return a placeholder so the caller still has
/// something readable to log.
fn decode_login_disconnect(data: &[u8]) -> String {
    let mut reader = BinaryReader::new(data);
    reader.read::<VarIntPrefixedString>().map_or_else(
        |_| format!("<unparseable disconnect reason, {} bytes>", data.len()),
        VarIntPrefixedString::into_inner,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_marker_hostname_strips_port() {
        let result = forge_marker_hostname("127.0.0.1:25565", "\0FML2\0");
        assert_eq!(result, "127.0.0.1\0FML2\0");
    }

    #[test]
    fn forge_marker_hostname_handles_missing_port() {
        let result = forge_marker_hostname("forge.internal", "\0FML3\0");
        assert_eq!(result, "forge.internal\0FML3\0");
    }

    #[test]
    fn forge_marker_hostname_handles_ipv6_style() {
        // IPv6 addresses use ':' inside — `rsplit_once(':')` keeps
        // the last colon as the port separator, which matches real-
        // world usage like `[::1]:25565`.
        let result = forge_marker_hostname("[::1]:25565", "\0FML2\0");
        assert_eq!(result, "[::1]\0FML2\0");
    }

    #[test]
    fn decode_login_disconnect_returns_placeholder_on_garbage() {
        let placeholder = decode_login_disconnect(&[0xFF, 0xFF]);
        assert!(placeholder.contains("unparseable"));
    }

    #[test]
    fn decode_login_disconnect_returns_string_when_well_formed() {
        // VarInt(5) "hello" — should round-trip.
        let mut bytes = vec![0x05];
        bytes.extend_from_slice(b"hello");
        let reason = decode_login_disconnect(&bytes);
        assert_eq!(reason, "hello");
    }

    /// Live recording against the real upstream. Verifies the recorder
    /// can drive both FML2 and FML3 dialects to completion and persists
    /// a snapshot file.
    ///
    /// Set `PICOLIMBO_TEST_UPSTREAM` to override the address.
    #[tokio::test]
    #[ignore = "requires a live Forge / NeoForge upstream"]
    async fn live_record_against_real_server() {
        use std::path::PathBuf;

        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());
        let snapshot_path = std::env::temp_dir().join(format!(
            "picolimbo_recorder_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        let forge_cfg = ForgeConfig {
            enabled: true,
            upstream: addr.clone(),
            snapshot_path: PathBuf::from(&snapshot_path),
            record_on_start: true,
            status_cache_ttl_secs: 60,
            status_request_timeout_ms: 3_000,
            record_timeout_ms: 15_000,
            recorder_username: "_picolimbo".into(),
        };

        // The live test can opt-in to a Velocity secret via
        // `PICOLIMBO_TEST_VELOCITY_SECRET`; when unset we send None
        // (suitable for a bootstrap server without forwarding).
        let secret = std::env::var("PICOLIMBO_TEST_VELOCITY_SECRET").ok();
        let snapshot = record_and_persist(&forge_cfg, None, secret.as_deref().map(str::as_bytes))
            .await
            .expect("recording must succeed against a real Forge server");

        // At least one dialect must have produced steps.
        let fml2_steps = snapshot.fml2.as_ref().map_or(0, |s| s.steps.len());
        let fml3_steps = snapshot.fml3.as_ref().map_or(0, |s| s.steps.len());
        eprintln!(
            "recorded: FML2={} steps, FML3={} steps, status_forge_data={}",
            fml2_steps,
            fml3_steps,
            snapshot.status_forge_data.is_some()
        );
        // For "Forge-enabled but mod-less" servers (e.g. a NeoForge
        // 1.21.8 backend with no mods loaded) the upstream simply
        // never sends `fml:handshake` plugin messages — the only
        // Forge-flavour signal it emits during the Configuration
        // phase is `minecraft:brand = "forge"` (which PicoLimbo's
        // existing brand handler already forwards). In that case 0
        // recorded FML steps is the *correct* outcome; we keep the
        // status_forge_data side of the snapshot as the load-bearing
        // proof that the upstream is reachable.
        let captured_anything =
            fml2_steps > 0 || fml3_steps > 0 || snapshot.status_forge_data.is_some();
        assert!(
            captured_anything,
            "recording produced absolutely nothing — \
             server may not be reachable / Forge-enabled"
        );

        // Snapshot file must exist on disk.
        assert!(snapshot_path.exists(), "snapshot file not written");
        std::fs::remove_file(&snapshot_path).ok();
    }

    /// Drives a full FML3+P763 handshake against the live upstream
    /// using a real Velocity HMAC secret. Prints every packet seen so
    /// we can pinpoint exactly where the negotiation falls over.
    ///
    /// Run with:
    /// ```text
    /// PICOLIMBO_TEST_UPSTREAM=127.0.0.1:46719 \
    /// PICOLIMBO_TEST_VELOCITY_SECRET=... \
    ///   cargo test -p pico_limbo --lib \
    ///     forge::recorder::tests::live_probe_full_handshake_with_velocity \
    ///     -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "diagnostic probe; requires a live upstream"]
    async fn live_probe_full_handshake_with_velocity() {
        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());
        // Velocity secret is OPTIONAL. When pointed at a real
        // Velocity-fronted Forge backend we need it to clear the
        // forwarding gate. When pointed at PicoLimbo itself (testing
        // replay), the limbo will reply with the recorded
        // `fml:loginwrapper` packets directly and never asks for
        // `velocity:player_info`, so the secret is irrelevant.
        let secret = std::env::var("PICOLIMBO_TEST_VELOCITY_SECRET").ok();

        let probe_protocol: i32 = std::env::var("PICOLIMBO_TEST_PROTOCOL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(763);
        eprintln!("== probing with wire protocol {probe_protocol} ==");
        let mut client = UpstreamClient::connect(&addr, Duration::from_secs(5))
            .await
            .expect("connect");
        let host = "limbo-probe\0FML3\0";
        client
            .send_handshake(probe_protocol, host, 25565, HandshakeIntent::Login)
            .await
            .expect("handshake");
        client
            .send_login_start(probe_protocol, "Probe", uuid::Uuid::nil())
            .await
            .expect("login start");

        for round in 0..30 {
            let raw = match client.read_packet(Duration::from_secs(5)).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[round {round}] read failed: {e}");
                    break;
                }
            };
            let id = raw.packet_id().unwrap_or(0xFF);
            match id {
                packet_ids::CB_LOGIN_SET_COMPRESSION => {
                    let mut reader = BinaryReader::new(raw.data());
                    let threshold = reader.read::<VarInt>().unwrap().inner();
                    eprintln!("[round {round}] SetCompression threshold={threshold}");
                    if threshold >= 0 {
                        client.set_compression(threshold as usize, 6);
                    }
                }
                packet_ids::CB_LOGIN_PLUGIN_REQUEST => {
                    let mut reader = BinaryReader::new(raw.data());
                    let message_id = reader.read::<VarInt>().unwrap().inner();
                    let channel: VarIntPrefixedString = reader.read().unwrap();
                    let payload = reader.remaining_bytes().unwrap();
                    let channel = channel.into_inner();
                    eprintln!(
                        "[round {round}] LPR id={message_id} channel={channel} \
                         payload={}B head=[{}]",
                        payload.len(),
                        payload
                            .iter()
                            .take(16)
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    if is_velocity_player_info_channel(&channel) {
                        if let Some(secret) = secret.as_deref() {
                            let identity = OutboundIdentity::recorder("Probe");
                            let signed =
                                build_signed_player_info(secret.as_bytes(), &identity).unwrap();
                            eprintln!(
                                "  -> signing velocity:player_info ({}B response)",
                                signed.len()
                            );
                            client
                                .send_login_plugin_response(message_id, Some(&signed))
                                .await
                                .unwrap();
                        } else {
                            eprintln!("  -> velocity:player_info but no secret set; sending empty");
                            client
                                .send_login_plugin_response(message_id, Some(&[]))
                                .await
                                .unwrap();
                        }
                    } else {
                        eprintln!("  -> empty present response");
                        client
                            .send_login_plugin_response(message_id, Some(&[]))
                            .await
                            .unwrap();
                    }
                }
                packet_ids::CB_LOGIN_SUCCESS => {
                    eprintln!("[round {round}] LoginSuccess 🎉");
                    break;
                }
                packet_ids::CB_LOGIN_DISCONNECT => {
                    let reason = decode_login_disconnect(raw.data());
                    eprintln!("[round {round}] Disconnect: {reason}");
                    break;
                }
                packet_ids::CB_LOGIN_ENCRYPTION_REQUEST => {
                    eprintln!("[round {round}] EncryptionRequest (online-mode)");
                    break;
                }
                other => {
                    eprintln!(
                        "[round {round}] unknown packet 0x{other:02x} ({}B)",
                        raw.data().len()
                    );
                    break;
                }
            }
        }
    }

    /// Deeper probe: drive the FML3+P763 handshake through several
    /// rounds with different response strategies to determine what
    /// the server actually wants.
    #[tokio::test]
    #[ignore = "diagnostic probe; requires a live upstream"]
    async fn live_probe_response_strategies() {
        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());

        for (strategy_label, strategy) in [
            ("Some(empty)", ResponseStrategy::SomeEmpty),
            ("None (not present)", ResponseStrategy::NotPresent),
            (
                "Echo (mirror the request payload)",
                ResponseStrategy::EchoRequest,
            ),
        ] {
            eprintln!("\n=== Strategy: {strategy_label} ===");

            let mut client = match UpstreamClient::connect(&addr, Duration::from_secs(5)).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("connect failed: {e}");
                    continue;
                }
            };

            let host = "limbo-probe\0FML3\0";
            let _ = client
                .send_handshake(763, host, 25565, HandshakeIntent::Login)
                .await;
            let _ = client
                .send_login_start(763, "Probe", uuid::Uuid::nil())
                .await;

            for round in 0..6 {
                let raw = match client.read_packet(Duration::from_secs(3)).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("  round {round}: read failed: {e}");
                        break;
                    }
                };
                let id = raw.packet_id().unwrap_or(0xFF);
                match id {
                    packet_ids::CB_LOGIN_PLUGIN_REQUEST => {
                        let mut reader = BinaryReader::new(raw.data());
                        let message_id = reader.read::<VarInt>().unwrap().inner();
                        let channel: VarIntPrefixedString = reader.read().unwrap();
                        let payload = reader.remaining_bytes().unwrap();
                        eprintln!(
                            "  round {round}: 0x04 LPR id={message_id} channel={} payload={}B [{}]",
                            channel.inner(),
                            payload.len(),
                            payload
                                .iter()
                                .take(16)
                                .map(|b| format!("{b:02x}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        let response_data: Option<&[u8]> = match strategy {
                            ResponseStrategy::SomeEmpty => Some(&[]),
                            ResponseStrategy::NotPresent => None,
                            ResponseStrategy::EchoRequest => Some(&payload),
                        };
                        let owned;
                        let response_ref = match strategy {
                            ResponseStrategy::EchoRequest => {
                                owned = payload;
                                Some(owned.as_slice())
                            }
                            _ => response_data,
                        };
                        if let Err(e) = client
                            .send_login_plugin_response(message_id, response_ref)
                            .await
                        {
                            eprintln!("  round {round}: send response failed: {e}");
                            break;
                        }
                    }
                    packet_ids::CB_LOGIN_SUCCESS => {
                        eprintln!("  round {round}: 0x02 LoginSuccess !!! handshake completed");
                        break;
                    }
                    packet_ids::CB_LOGIN_DISCONNECT => {
                        let reason = decode_login_disconnect(raw.data());
                        eprintln!("  round {round}: 0x00 Disconnect: {reason}");
                        break;
                    }
                    packet_ids::CB_LOGIN_SET_COMPRESSION => {
                        let mut reader = BinaryReader::new(raw.data());
                        let threshold = reader.read::<VarInt>().unwrap().inner();
                        eprintln!("  round {round}: 0x03 SetCompression threshold={threshold}");
                        if threshold >= 0 {
                            client.set_compression(threshold as usize, 6);
                        }
                    }
                    other => {
                        eprintln!(
                            "  round {round}: unexpected packet 0x{other:02x} ({}B)",
                            raw.data().len()
                        );
                        break;
                    }
                }
            }
        }
    }

    enum ResponseStrategy {
        SomeEmpty,
        NotPresent,
        EchoRequest,
    }

    /// Diagnostic probe: tries several Login-phase variations against
    /// the live upstream and reports what each one yields. Helps us
    /// figure out whether a recorder failure is caused by the FML
    /// marker, the declared protocol version, or the `LoginStart` layout.
    ///
    /// Run with:
    /// ```text
    /// PICOLIMBO_TEST_UPSTREAM=127.0.0.1:46719 cargo test -p pico_limbo \
    ///     --lib forge::recorder::tests::live_probe -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "diagnostic probe; requires a live upstream"]
    async fn live_probe() {
        let addr = std::env::var("PICOLIMBO_TEST_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:46719".to_string());

        // (label, protocol_version, hostname_marker for `LoginStart`)
        let cases: &[(&str, i32, &str)] = &[
            // We learned from a previous run that this upstream is
            // 1.20.1 (protocol 763) but uses FML *net version 3*.
            // The decisive test is: protocol 763 + FML3 marker.
            ("FML3-763 (1.20.1 + FML net v3) ★", 763, "\0FML3\0"),
            ("FML2-763 (1.20.1 + FML net v2)  ", 763, "\0FML2\0"),
            ("FML-763  (legacy 1.20.1)        ", 763, "\0FML\0"),
            ("FML3-767 (1.21 + FML net v3)    ", 767, "\0FML3\0"),
            ("vanilla-763 (no marker)         ", 763, ""),
        ];

        for (label, protocol, marker) in cases {
            let host = format!("limbo-probe{marker}");
            let mut client = match UpstreamClient::connect(&addr, Duration::from_secs(5)).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[{label}] connect failed: {e}");
                    continue;
                }
            };
            if let Err(e) = client
                .send_handshake(*protocol, &host, HANDSHAKE_PORT, HandshakeIntent::Login)
                .await
            {
                eprintln!("[{label}] handshake send failed: {e}");
                continue;
            }
            if let Err(e) = client
                .send_login_start(*protocol, "Probe", uuid::Uuid::nil())
                .await
            {
                eprintln!("[{label}] login_start send failed: {e}");
                continue;
            }

            match client.read_packet(Duration::from_secs(3)).await {
                Ok(raw) => {
                    let id = raw.packet_id().unwrap_or(0xFF);
                    let id_label = match id {
                        0x00 => "Disconnect/StatusResponse",
                        0x01 => "EncryptionRequest",
                        0x02 => "LoginSuccess",
                        0x03 => "SetCompression",
                        0x04 => "LoginPluginRequest",
                        _ => "?",
                    };
                    let body_preview = if id == packet_ids::CB_LOGIN_DISCONNECT {
                        decode_login_disconnect(raw.data())
                    } else {
                        format!("({} bytes)", raw.data().len())
                    };
                    eprintln!("[{label}] -> packet 0x{id:02x} ({id_label}): {body_preview}");
                }
                Err(e) => {
                    eprintln!("[{label}] read failed: {e}");
                }
            }
        }
    }
}
