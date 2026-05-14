use crate::configuration::TaggedForwarding;
use crate::configuration::boss_bar::BossBarConfig;
use crate::configuration::config::{Config, ConfigError, load_or_create};
use crate::configuration::forge::ForgeConfig;
use crate::configuration::tab_list::TabListMode;
use crate::configuration::title::TitleConfig;
use crate::configuration::world_config::boundaries::BoundariesConfig;
use crate::forge::recorder::record_and_persist;
use crate::forge::snapshot::Snapshot;
use crate::forge::snapshot_io::{LoadOutcome, load_snapshot};
use crate::forge::status_proxy::ForgeStatusCache;
use crate::server::network::Server;
use crate::server_state::{ServerState, ServerStateBuilderError};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{Level, debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub async fn start_server(
    config_path: PathBuf,
    logging_level: u8,
    token: Option<&CancellationToken>,
) -> ExitCode {
    enable_logging(logging_level);
    let Some(cfg) = load_configuration(&config_path) else {
        return ExitCode::FAILURE;
    };

    let bind = cfg.bind.clone();

    // Extract the Modern Forwarding secret (if any) up front — the
    // Forge recorder needs it to play the Velocity role against the
    // upstream backend, but the full `ForwardingConfig` is moved into
    // `build_state` below, so we have to take it before that.
    let velocity_secret = extract_velocity_secret(&cfg);

    // If the Forge bridge is enabled with `record_on_start`, run a
    // recording session now (before opening the listening socket). The
    // resulting snapshot is then loaded by `build_state` together with
    // the rest of the configuration. Failure is non-fatal: limbo
    // proceeds with whatever snapshot (if any) is already on disk.
    maybe_record_forge_snapshot(&cfg.forge, velocity_secret.as_deref()).await;

    match build_state(cfg) {
        Ok(server_state) => {
            Server::new(&bind, server_state).run(token).await;
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!("Failed to start PicoLimbo: {err}");
            ExitCode::SUCCESS
        }
    }
}

/// Reads back the Velocity Modern Forwarding secret declared in the
/// limbo's own `[forwarding]` config. Returns `None` when forwarding
/// is in a non-modern mode.
///
/// The `ForwardingConfig` enum is `untagged`, so `TaggedForwarding`'s
/// `From<ForwardingConfig>` impl normalises it for us. We have to
/// borrow-then-match because we can't move out of `cfg.forwarding`
/// (it's consumed by `build_state` later).
fn extract_velocity_secret(cfg: &Config) -> Option<Vec<u8>> {
    // Inspect without cloning by destructuring the borrowed enum.
    use crate::configuration::forwarding::{ForwardingConfig, TaggedForwarding};
    match &cfg.forwarding {
        ForwardingConfig::Tagged(TaggedForwarding::Modern { secret }) => {
            Some(secret.as_bytes().to_vec())
        }
        ForwardingConfig::Structured(s) => {
            // Mirror the same fallback logic the `From` impl uses:
            // a structured config with velocity.enabled = true and
            // a secret is equivalent to Tagged::Modern.
            let v = s.velocity_view();
            if v.enabled() && !v.secret().is_empty() {
                Some(v.secret().as_bytes().to_vec())
            } else {
                None
            }
        }
        ForwardingConfig::Tagged(_) => None,
    }
}

/// Runs a Forge handshake recording session against the configured
/// upstream when `forge.enabled = true && forge.record_on_start =
/// true`. The session is best-effort: any failure is logged and we
/// keep going.
async fn maybe_record_forge_snapshot(forge_cfg: &ForgeConfig, velocity_secret: Option<&[u8]>) {
    if !forge_cfg.enabled || !forge_cfg.record_on_start {
        return;
    }

    info!(
        "Forge bridge: starting recording session against {} (record_on_start)",
        forge_cfg.upstream
    );

    match record_and_persist(forge_cfg, None, velocity_secret).await {
        Ok(snapshot) => {
            let fml2 = snapshot.fml2.as_ref().map_or(0, |s| s.steps.len());
            let fml3 = snapshot.fml3.as_ref().map_or(0, |s| s.steps.len());
            info!(
                "Forge bridge: recording session complete (FML2: {} steps, FML3: {} steps)",
                fml2, fml3
            );
        }
        Err(e) => {
            warn!(
                "Forge bridge: recording session failed: {e}; \
                 limbo will continue with the on-disk snapshot (if any)"
            );
        }
    }
}

fn load_configuration(config_path: &PathBuf) -> Option<Config> {
    let cfg = load_or_create(config_path);
    match cfg {
        Err(ConfigError::TomlDeserialize(message, ..)) => {
            error!("Failed to load configuration: {}", message);
        }
        Err(ConfigError::Io(message, ..)) => {
            error!("Failed to load configuration: {}", message);
        }
        Err(ConfigError::EnvPlaceholder(var)) => {
            error!("Failed to load configuration: {}", var);
        }
        Err(ConfigError::TomlSerialize(message, ..)) => {
            error!("Failed to save default configuration file: {}", message);
        }
        Ok(cfg) => return Some(cfg),
    }
    None
}

fn build_state(cfg: Config) -> Result<ServerState, ServerStateBuilderError> {
    let mut server_state_builder = ServerState::builder();

    let forwarding: TaggedForwarding = cfg.forwarding.into();

    match forwarding {
        TaggedForwarding::None => {
            server_state_builder.disable_forwarding();
        }
        TaggedForwarding::Legacy => {
            debug!("Enabling legacy forwarding");
            server_state_builder.enable_legacy_forwarding();
        }
        TaggedForwarding::BungeeGuard { tokens } => {
            server_state_builder.enable_bungee_guard_forwarding(tokens);
        }
        TaggedForwarding::Modern { secret } => {
            debug!("Enabling modern forwarding");
            server_state_builder.enable_modern_forwarding(secret);
        }
    }

    if let BoundariesConfig::Enabled(boundaries) = cfg.world.boundaries {
        if cfg.world.spawn_position.1 < f64::from(boundaries.min_y) {
            return Err(ServerStateBuilderError::InvalidSpawnPosition);
        }
        server_state_builder.boundaries(boundaries.min_y, boundaries.teleport_message)?;
    }

    if let TabListMode::Enabled(ref tab_list) = cfg.tab_list.mode {
        server_state_builder.tab_list(&tab_list.header, &tab_list.footer)?;
    }

    if let BossBarConfig::Enabled(boss_bar) = cfg.boss_bar {
        server_state_builder.boss_bar(boss_bar)?;
    }

    if let TitleConfig::Enabled(title) = cfg.title {
        server_state_builder.title(
            &title.title,
            &title.subtitle,
            title.fade_in,
            title.stay,
            title.fade_out,
        )?;
    }

    let server_icon = cfg.server_list.server_icon;
    if std::fs::exists(&server_icon)? {
        server_state_builder.fav_icon(server_icon)?;
    }

    if cfg.forge.enabled {
        let forge_cache = build_forge_status_cache(&cfg.forge);
        server_state_builder.forge_status_cache(Some(forge_cache));

        // Load the recorded handshake snapshot for the replay state
        // machine. A missing / corrupt snapshot is non-fatal: vanilla
        // clients keep working and Forge clients are politely turned
        // away (see login_start.rs for the fallback path).
        if let Some(snapshot) = load_full_snapshot(&cfg.forge.snapshot_path) {
            server_state_builder.forge_snapshot(Some(Arc::new(snapshot)));
        }
    }

    server_state_builder
        .dimension(cfg.world.dimension.into())
        .time_world(cfg.world.time.into())
        .lock_time(cfg.world.experimental.lock_time)
        .description_text(&cfg.server_list.message_of_the_day)
        .welcome_message(&cfg.welcome_message)
        .action_bar(&cfg.action_bar)?
        .max_players(cfg.server_list.max_players)
        .show_online_player_count(cfg.server_list.show_online_player_count)
        .game_mode(cfg.default_game_mode.into())
        .hardcore(cfg.hardcore)
        .spawn_position(cfg.world.spawn_position)
        .spawn_rotation(cfg.world.spawn_rotation)
        .view_distance(cfg.world.experimental.view_distance)
        .schematic(cfg.world.experimental.schematic_file)
        .enable_compression(cfg.compression.threshold, cfg.compression.level)?
        .fetch_player_skins(cfg.fetch_player_skins)
        .reduced_debug_info(cfg.reduced_debug_info)
        .set_player_listed(cfg.tab_list.player_listed)
        .set_reply_to_status(cfg.server_list.reply_to_status)
        .set_allow_unsupported_versions(cfg.allow_unsupported_versions)
        .set_allow_flight(cfg.allow_flight)
        .set_accept_transfers(cfg.accept_transfers)
        .server_commands(cfg.commands);

    server_state_builder.build()
}

/// Instantiates the [`ForgeStatusCache`], best-effort loading any
/// previously-recorded snapshot from disk for use as the fallback when
/// the upstream is unreachable. A missing or corrupt snapshot is *not*
/// fatal — the limbo still starts, it just has nothing to serve from
/// cold cache.
fn build_forge_status_cache(forge_cfg: &ForgeConfig) -> Arc<ForgeStatusCache> {
    info!(
        "Forge bridge enabled, upstream={}, snapshot={}",
        forge_cfg.upstream,
        forge_cfg.snapshot_path.display()
    );

    let snapshot_fallback = load_snapshot_fallback(&forge_cfg.snapshot_path);
    let cfg_arc = Arc::new(forge_cfg.clone());
    Arc::new(ForgeStatusCache::new(cfg_arc, snapshot_fallback))
}

/// Returns the `status_forge_data` field from the on-disk snapshot at
/// `path`, if any. All failure modes degrade gracefully into `None` —
/// the cache will then fall back to a live upstream fetch only.
fn load_snapshot_fallback(path: &std::path::Path) -> Option<serde_json::Value> {
    match load_snapshot(path) {
        Ok(LoadOutcome::Loaded(snapshot)) => {
            let snapshot: Snapshot = *snapshot;
            if snapshot.status_forge_data.is_some() {
                debug!(
                    "Loaded Forge snapshot from {} (captured {}, upstream {})",
                    path.display(),
                    snapshot.captured_at_unix,
                    snapshot.upstream_addr,
                );
            }
            snapshot.status_forge_data
        }
        Ok(LoadOutcome::Missing) => {
            debug!(
                "Forge snapshot not found at {}; will fetch live on first ping",
                path.display()
            );
            None
        }
        Err(e) => {
            warn!(
                "Forge snapshot at {} could not be loaded: {}; continuing with live fetch only",
                path.display(),
                e
            );
            None
        }
    }
}

/// Returns the *full* on-disk snapshot (including FML2/FML3 handshake
/// step recordings) for use by the replay state machine. Logs a brief
/// summary on success, and degrades to `None` on any I/O / parse
/// failure (forge replay simply won't engage for those clients).
fn load_full_snapshot(path: &std::path::Path) -> Option<Snapshot> {
    match load_snapshot(path) {
        Ok(LoadOutcome::Loaded(snapshot)) => {
            let snapshot: Snapshot = *snapshot;
            let fml2_steps = snapshot.fml2.as_ref().map_or(0, |s| s.steps.len());
            let fml3_steps = snapshot.fml3.as_ref().map_or(0, |s| s.steps.len());
            info!(
                "Loaded Forge snapshot from {} (FML2 steps: {}, FML3 steps: {})",
                path.display(),
                fml2_steps,
                fml3_steps
            );
            Some(snapshot)
        }
        Ok(LoadOutcome::Missing) => {
            debug!(
                "Forge snapshot not found at {}; replay disabled",
                path.display()
            );
            None
        }
        Err(e) => {
            warn!(
                "Forge snapshot at {} unreadable: {}; replay disabled",
                path.display(),
                e
            );
            None
        }
    }
}

fn enable_logging(verbose: u8) {
    let log_level = match verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(log_level.into()))
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
}
