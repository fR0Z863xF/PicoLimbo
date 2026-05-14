use crate::forge::replay::should_replay;
use crate::handlers::configuration::send_play_packets;
use crate::kick_messages::CLIENT_MODERN_FORWARDING_NOT_SUPPORTED_KICK_MESSAGE;
use crate::server::batch::Batch;
use crate::server::client_state::ClientState;
use crate::server::game_profile::GameProfile;
use crate::server::packet_handler::{PacketHandler, PacketHandlerError};
use crate::server::packet_registry::PacketRegistry;
use crate::server_state::ServerState;
use minecraft_packets::login::custom_query_packet::CustomQueryPacket;
use minecraft_packets::login::game_profile_packet::GameProfilePacket;
use minecraft_packets::login::login_state_packet::LoginStartPacket;
use minecraft_packets::login::login_success_packet::LoginSuccessPacket;
use minecraft_packets::login::set_compression_packet::SetCompressionPacket;
use minecraft_protocol::prelude::{Identifier, ProtocolVersion, VarInt};
use rand::RngExt;

impl PacketHandler for LoginStartPacket {
    fn handle(
        &self,
        client_state: &mut ClientState,
        server_state: &ServerState,
    ) -> Result<Batch<PacketRegistry>, PacketHandlerError> {
        let mut batch = Batch::new();

        // Forge replay takes precedence over the vanilla / Velocity
        // paths: a Forge client is identified by the marker on the
        // Handshake hostname (already stashed on `client_state`), and
        // we have a snapshot to replay back at it. When *both* of those
        // are true, we drive the FML2 plugin-message exchange first
        // and only fall through to LoginSuccess once the snapshot is
        // exhausted.
        if should_replay(client_state.forge_kind())
            && let Some(snapshot) = server_state.forge_snapshot()
            && let Some(fml2) = snapshot.fml2.as_ref()
            && !fml2.steps.is_empty()
        {
            // Capture the client's identity now — we will fire
            // LoginSuccess for it once the replay completes.
            let game_profile: GameProfile = self.into();
            client_state.set_game_profile(game_profile);

            client_state.start_forge_replay();
            // Push the first recorded step.
            if let Some(session) = client_state.forge_session_mut() {
                let _ = enqueue_next_forge_step(&mut batch, session, fml2);
            }
            return Ok(batch);
        }

        if server_state.is_modern_forwarding() {
            if client_state.protocol_version().is_modern() {
                login_start_velocity(&mut batch, client_state);
            } else {
                client_state.kick(CLIENT_MODERN_FORWARDING_NOT_SUPPORTED_KICK_MESSAGE);
            }
        } else {
            let game_profile: GameProfile = self.into();
            fire_login_success(&mut batch, client_state, server_state, game_profile)?;
        }
        Ok(batch)
    }
}

/// Picks the next recorded snapshot step (if any) and queues a
/// clientbound `CustomQueryPacket` carrying its channel + payload.
/// Returns `Ok(true)` when a step was queued; `Ok(false)` if the
/// snapshot is exhausted.
pub fn enqueue_next_forge_step(
    batch: &mut Batch<PacketRegistry>,
    session: &mut crate::forge::replay::Fml2ReplaySession,
    snapshot: &crate::forge::snapshot::Fml2Snapshot,
) -> bool {
    let Some((message_id, step)) = session.take_next_step(snapshot) else {
        return false;
    };
    // The recorded `channel` is a free-form string (Forge uses
    // `fml:loginwrapper` and `fml:handshake`, NeoForge uses similar).
    // We bypass the strict `Identifier::new` parser and reuse
    // `Identifier::new_unchecked` since we are forwarding bytes
    // captured from a known-good upstream.
    let identifier = parse_channel_identifier(&step.channel);
    let packet = CustomQueryPacket {
        message_id: VarInt::new(message_id),
        channel: identifier,
        data: step.payload.clone(),
    };
    batch.queue(move || PacketRegistry::CustomQuery(packet));
    true
}

/// Splits a channel string of the form `namespace:value` into an
/// [`Identifier`] using the unchecked constructor (Forge channels
/// occasionally contain characters outside the strict identifier
/// grammar). Falls back to the `fml:handshake` placeholder if the
/// channel string is malformed.
fn parse_channel_identifier(channel: &str) -> Identifier {
    if let Some((ns, val)) = channel.split_once(':') {
        Identifier::new_unchecked(ns, val)
    } else {
        Identifier::new_unchecked("fml", "handshake")
    }
}

fn login_start_velocity(batch: &mut Batch<PacketRegistry>, client_state: &mut ClientState) {
    let message_id = {
        let mut rng = rand::rng();
        rng.random()
    };
    client_state.set_velocity_login_message_id(message_id);
    let packet = CustomQueryPacket::velocity_info_channel(message_id);
    batch.queue(|| PacketRegistry::CustomQuery(packet));
}

pub fn fire_login_success(
    batch: &mut Batch<PacketRegistry>,
    client_state: &mut ClientState,
    server_state: &ServerState,
    game_profile: GameProfile,
) -> Result<(), PacketHandlerError> {
    let protocol_version = client_state.protocol_version();

    if protocol_version.is_after_inclusive(ProtocolVersion::V1_8)
        && let Some(compression_settings) = server_state.compression_settings()
    {
        let threshold = compression_settings.threshold;
        let packet = SetCompressionPacket::new(i32::try_from(threshold)?);
        batch.queue(|| PacketRegistry::SetCompression(packet));
    }

    if protocol_version.is_after_inclusive(ProtocolVersion::V1_21_2) {
        let packet = LoginSuccessPacket::new(game_profile.uuid(), game_profile.username());
        batch.queue(|| PacketRegistry::LoginSuccess(packet));
    } else {
        let packet = GameProfilePacket::new(game_profile.uuid(), game_profile.username());
        batch.queue(|| PacketRegistry::GameProfile(packet));
    }

    client_state.set_game_profile(game_profile);

    if !protocol_version.supports_configuration_state() {
        send_play_packets(batch, client_state, server_state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use minecraft_protocol::prelude::{ProtocolVersion, State};

    fn vanilla() -> ServerState {
        ServerState::builder().build().unwrap()
    }

    fn velocity() -> ServerState {
        let mut builder = ServerState::builder();
        let secret = "foo";
        builder.enable_modern_forwarding(secret);
        builder.build().unwrap()
    }

    pub fn client(protocol: ProtocolVersion) -> ClientState {
        let mut cs = ClientState::default();
        cs.set_protocol_version(protocol);
        cs.set_state(State::Login);
        cs
    }

    fn packet() -> LoginStartPacket {
        LoginStartPacket::default()
    }

    // modern forwarding
    #[tokio::test]
    async fn test_login_start_velocity_happy_path() {
        // Given
        let server_state = velocity();
        let mut client_state = client(ProtocolVersion::V1_13); // ≥ 1.13
        let pkt = packet();

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();
        let mut batch = batch.into_stream();

        // Then
        assert!(
            matches!(batch.next().await.unwrap(), PacketRegistry::CustomQuery(_)),
            "first packet should be the velocity CustomQuery"
        );
        assert_ne!(client_state.get_velocity_login_message_id(), -1);
        assert!(client_state.should_kick().is_none());
        assert!(batch.next().await.is_none());
    }

    #[tokio::test]
    async fn test_login_start_velocity_kicks_old_client() {
        // Given
        let server_state = velocity();
        let mut client_state = client(ProtocolVersion::V1_12_2); // < 1.13
        let pkt = packet();

        // When
        let result = pkt.handle(&mut client_state, &server_state);

        // Then
        assert!(result.is_ok());
        assert_eq!(
            client_state.should_kick(),
            Some(CLIENT_MODERN_FORWARDING_NOT_SUPPORTED_KICK_MESSAGE.to_string())
        );
    }

    // vanilla login
    #[tokio::test]
    async fn test_login_start_vanilla_newer_than_1_21_2() {
        // Given
        let server_state = vanilla();
        let mut client_state = client(ProtocolVersion::V1_21_2);
        let pkt = packet();

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();
        let mut batch = batch.into_stream();

        // Then
        assert!(
            matches!(batch.next().await.unwrap(), PacketRegistry::LoginSuccess(_)),
            "first packet should be LoginSuccess for ≥ 1.21.2"
        );
    }

    #[tokio::test]
    async fn test_login_start_vanilla_before_1_21_2() {
        // Given
        let server_state = vanilla();
        let mut client_state = client(ProtocolVersion::V1_20_2);
        let pkt = packet();

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();
        let mut batch = batch.into_stream();

        // Then
        assert!(
            matches!(batch.next().await.unwrap(), PacketRegistry::GameProfile(_)),
            "first packet should be GameProfile for < 1.21.2"
        );
    }

    #[tokio::test]
    async fn test_should_not_send_play_packets_when_configuration_state_was_introduced() {
        // Given
        let server_state = vanilla();
        let mut client_state = client(ProtocolVersion::V1_20_2);
        let pkt = packet();

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();
        let mut batch = batch.into_stream();

        // Then
        let _ = batch.next().await.unwrap();
        assert!(batch.next().await.is_none());
    }

    #[tokio::test]
    async fn test_should_send_play_packets_for_versions_prior_to_configuration_state() {
        // Given
        let server_state = vanilla();
        let mut client_state = client(ProtocolVersion::V1_20);
        let pkt = packet();

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();
        let mut batch = batch.into_stream();

        // Then
        let _ = batch.next().await.unwrap();
        assert!(batch.next().await.is_some());
    }
}
