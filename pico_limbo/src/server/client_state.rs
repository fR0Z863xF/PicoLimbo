use crate::forge::replay::{Fml2ReplaySession, Fml3ReplaySession};
use crate::server::game_profile::GameProfile;
use minecraft_packets::handshaking::handshake_packet::ForgeKind;
use minecraft_packets::login::Property;
use minecraft_protocol::prelude::{ProtocolVersion, State, Uuid};
use tracing::info;

#[derive(PartialEq, Eq)]
pub enum KeepAliveStatus {
    Disabled,
    ShouldEnable,
    Enabled,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            state: State::Handshake,
            protocol_version: ProtocolVersion::Any,
            kick_message: None,
            message_id: -1,
            game_profile: None,
            keep_alive_enabled: KeepAliveStatus::Disabled,
            feet_y: 0.0,
            is_flight_allowed: false,
            is_flying: false,
            flying_speed: 0.05,
            forge_kind: ForgeKind::None,
            forge_session: None,
            forge_fml3_session: None,
        }
    }
}

pub struct ClientState {
    state: State,
    protocol_version: ProtocolVersion,
    kick_message: Option<String>,
    message_id: i32,
    game_profile: Option<GameProfile>,
    keep_alive_enabled: KeepAliveStatus,
    feet_y: f64,
    is_flight_allowed: bool,
    is_flying: bool,
    flying_speed: f32,
    /// Forge dialect detected from the connecting client's Handshake
    /// hostname suffix. `ForgeKind::None` for vanilla clients.
    forge_kind: ForgeKind,
    /// Active Forge FML2 (Login-phase) replay session, if the
    /// connection is in the middle of one. `None` until
    /// [`Self::start_forge_replay`] is called and remains `None` for
    /// vanilla / FML1 clients.
    forge_session: Option<Fml2ReplaySession>,
    /// Active Forge FML3 (Configuration-phase) replay session, used
    /// for 1.20.2+ clients. `None` until
    /// [`Self::start_forge_fml3_replay`] is called.
    forge_fml3_session: Option<Fml3ReplaySession>,
}

impl ClientState {
    const ANONYMOUS: &'static str = "Anonymous";

    // Kick

    pub fn kick(&mut self, kick_message: &str) {
        self.kick_message = Some(kick_message.to_string());
    }

    pub fn should_kick(&self) -> Option<String> {
        self.kick_message.clone()
    }

    // State

    pub const fn state(&self) -> State {
        self.state
    }

    pub const fn set_state(&mut self, new_state: State) {
        self.state = new_state;
    }

    // Protocol version

    pub const fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub const fn set_protocol_version(&mut self, new_protocol_version: ProtocolVersion) {
        self.protocol_version = new_protocol_version;
    }

    // Velocity

    pub const fn set_velocity_login_message_id(&mut self, message_id: i32) {
        self.message_id = message_id;
    }

    pub const fn get_velocity_login_message_id(&self) -> i32 {
        self.message_id
    }

    // Game profile

    pub fn set_game_profile(&mut self, game_profile: GameProfile) {
        if let Some(ref mut existing_game_profile) = self.game_profile {
            existing_game_profile.set_name(&game_profile.username());
        } else {
            self.game_profile = Some(game_profile);
        }

        if let Some(ref existing_game_profile) = self.game_profile
            && !existing_game_profile.is_anonymous()
        {
            info!(
                "UUID of player {} is {}",
                existing_game_profile.username(),
                existing_game_profile.uuid()
            );
        }
    }

    pub fn game_profile(&self) -> Option<GameProfile> {
        self.game_profile.clone()
    }

    pub fn get_username(&self) -> String {
        self.game_profile().map_or_else(
            || Self::ANONYMOUS.to_owned(),
            |profile| profile.username().to_owned(),
        )
    }

    pub fn get_unique_id(&self) -> Uuid {
        self.game_profile()
            .map_or_else(Uuid::default, |profile| profile.uuid())
    }

    pub fn get_textures(&self) -> Option<Property> {
        self.game_profile()
            .and_then(|profile| profile.textures().cloned())
    }

    // Keep alive

    pub fn should_enable_keep_alive(&self) -> bool {
        self.keep_alive_enabled == KeepAliveStatus::ShouldEnable
    }

    pub fn set_keep_alive_should_enable(&mut self) {
        if self.keep_alive_enabled == KeepAliveStatus::Disabled {
            self.keep_alive_enabled = KeepAliveStatus::ShouldEnable;
        }
    }

    pub fn set_keep_alive_enabled(&mut self) {
        if self.keep_alive_enabled == KeepAliveStatus::ShouldEnable {
            self.keep_alive_enabled = KeepAliveStatus::Enabled;
        }
    }

    // Position

    pub const fn get_y_position(&self) -> f64 {
        self.feet_y
    }

    pub const fn set_feet_position(&mut self, feet_y: f64) {
        self.feet_y = feet_y;
    }

    // Movement

    pub const fn is_flight_allowed(&self) -> bool {
        self.is_flight_allowed
    }

    pub const fn set_is_flight_allowed(&mut self, allow_flight: bool) {
        self.is_flight_allowed = allow_flight;
    }

    pub const fn is_flying(&self) -> bool {
        self.is_flying
    }

    pub const fn set_is_flying(&mut self, is_flying: bool) {
        self.is_flying = is_flying;
    }

    pub const fn get_flying_speed(&self) -> f32 {
        self.flying_speed
    }

    pub const fn set_flying_speed(&mut self, flying_speed: f32) {
        self.flying_speed = flying_speed;
    }

    // Forge

    /// Returns the Forge dialect declared by this client.
    pub const fn forge_kind(&self) -> ForgeKind {
        self.forge_kind
    }

    /// Records the Forge dialect detected from the Handshake hostname.
    pub const fn set_forge_kind(&mut self, kind: ForgeKind) {
        self.forge_kind = kind;
    }

    /// Mutably borrows the active Forge replay session, if any.
    /// Returns `None` if no replay is in progress (e.g. vanilla
    /// connection or replay already completed).
    pub const fn forge_session_mut(&mut self) -> Option<&mut Fml2ReplaySession> {
        self.forge_session.as_mut()
    }

    /// Initialises a fresh FML2 replay session. Used by the
    /// `LoginStart` handler when the connecting client carries an
    /// `\0FML2\0` / `\0FML3\0` marker and an on-disk snapshot is
    /// available to replay.
    pub fn start_forge_replay(&mut self) {
        self.forge_session = Some(Fml2ReplaySession::new());
    }

    /// Tears down any active Forge replay session. Called once the
    /// snapshot has been fully drained and `LoginSuccess` has been
    /// fired, so subsequent inbound `CustomQueryAnswer` packets fall
    /// through to the (currently velocity-only) default handler.
    pub fn finish_forge_replay(&mut self) {
        self.forge_session = None;
    }

    /// Mutably borrows the active FML3 replay session, if any.
    pub const fn forge_fml3_session_mut(&mut self) -> Option<&mut Fml3ReplaySession> {
        self.forge_fml3_session.as_mut()
    }

    /// Initialises a fresh FML3 replay session. Called by the
    /// `LoginAcknowledged` handler when the connecting client carries
    /// an FML3 marker and an on-disk snapshot has FML3 steps.
    #[allow(dead_code)] // Wired up by Step 9.5 configuration handler.
    pub fn start_forge_fml3_replay(&mut self) {
        self.forge_fml3_session = Some(Fml3ReplaySession::new());
    }

    /// Tears down the FML3 session once `FinishConfiguration` has
    /// been pushed and the connection has graduated to Play.
    #[allow(dead_code)] // Wired up by Step 9.5 configuration handler.
    pub fn finish_forge_fml3_replay(&mut self) {
        self.forge_fml3_session = None;
    }
}
