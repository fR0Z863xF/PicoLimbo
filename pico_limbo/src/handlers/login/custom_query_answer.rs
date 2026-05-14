use crate::forwarding::check_velocity_key_integrity::read_velocity_key;
use crate::forwarding::forwarding_result::ModernForwardingResult;
use crate::handlers::login::login_start::{enqueue_next_forge_step, fire_login_success};
use crate::kick_messages::PROXY_REQUIRED_KICK_MESSAGE;
use crate::server::batch::Batch;
use crate::server::client_state::ClientState;
use crate::server::game_profile::GameProfile;
use crate::server::packet_handler::{PacketHandler, PacketHandlerError};
use crate::server::packet_registry::PacketRegistry;
use crate::server_state::ServerState;
use minecraft_packets::login::custom_query_answer_packet::CustomQueryAnswerPacket;
use minecraft_protocol::prelude::BinaryReader;

impl PacketHandler for CustomQueryAnswerPacket {
    fn handle(
        &self,
        client_state: &mut ClientState,
        server_state: &ServerState,
    ) -> Result<Batch<PacketRegistry>, PacketHandlerError> {
        let mut batch = Batch::new();

        // First check: is this a reply to a Forge FML2 replay step?
        // The session's `pending` map only contains ids minted by the
        // replay state machine, so a hit definitively means "Forge
        // path"; a miss falls through to the existing Velocity logic.
        let recognised_forge_response = client_state
            .forge_session_mut()
            .is_some_and(|session| session.consume_response(self.message_id.inner()).is_some());

        if recognised_forge_response {
            // Push the next snapshot step if any remain; otherwise
            // declare the handshake done and fire LoginSuccess.
            let snapshot = server_state.forge_snapshot();
            let fml2 = snapshot.as_ref().and_then(|s| s.fml2.as_ref());

            let advanced = match (fml2, client_state.forge_session_mut()) {
                (Some(fml2), Some(session)) => {
                    enqueue_next_forge_step(&mut batch, session, fml2)
                }
                _ => false,
            };

            if !advanced {
                // Snapshot exhausted — graduate the connection.
                client_state.finish_forge_replay();
                if let Some(game_profile) = client_state.game_profile() {
                    fire_login_success(&mut batch, client_state, server_state, game_profile)?;
                } else {
                    // No GameProfile means LoginStart never queued
                    // one — should not happen on the replay path.
                    return Err(PacketHandlerError::custom(
                        "Forge replay finished without a captured GameProfile",
                    ));
                }
            }
            return Ok(batch);
        }

        let client_message_id = client_state.get_velocity_login_message_id();

        if server_state.is_modern_forwarding() && self.message_id.inner() == client_message_id {
            let secret_key = server_state
                .secret_key()
                .map_err(|_| PacketHandlerError::custom("No secret key"))?;
            let mut reader = BinaryReader::new(&self.data);
            let velocity_key = read_velocity_key(&mut reader, &secret_key);

            match velocity_key {
                ModernForwardingResult::Valid {
                    player_uuid,
                    player_name,
                    textures,
                } => {
                    let game_profile = GameProfile::new(&player_name, player_uuid, textures);
                    fire_login_success(&mut batch, client_state, server_state, game_profile)?;
                }
                ModernForwardingResult::Invalid => {
                    client_state.kick(PROXY_REQUIRED_KICK_MESSAGE);
                }
            }
        }
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use minecraft_protocol::prelude::{ProtocolVersion, VarInt};

    fn velocity() -> ServerState {
        let mut builder = ServerState::builder();
        builder.enable_modern_forwarding("foo");
        builder.build().unwrap()
    }

    fn client() -> ClientState {
        let mut cs = ClientState::default();
        cs.set_protocol_version(ProtocolVersion::V1_13);
        cs
    }

    fn packet(id: i32, data: Vec<u8>) -> CustomQueryAnswerPacket {
        CustomQueryAnswerPacket {
            message_id: VarInt::new(id),
            is_present: true,
            data,
        }
    }

    #[tokio::test]
    async fn test_custom_query_answer_kicks_on_invalid_key() {
        // Given
        let server_state = velocity();
        let mut client_state = client();

        let message_id = 42;
        client_state.set_velocity_login_message_id(message_id);

        let pkt = packet(message_id, vec![]);

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();

        // Then
        assert_eq!(
            client_state.should_kick(),
            Some(PROXY_REQUIRED_KICK_MESSAGE.to_string())
        );
        assert!(batch.into_stream().next().await.is_none());
    }

    #[tokio::test]
    async fn test_custom_query_answer_ignored_on_mismatching_id() {
        // Given
        let server_state = velocity();
        let mut client_state = client();
        client_state.set_velocity_login_message_id(10);

        let pkt = packet(11, vec![]);

        // When
        let batch = pkt.handle(&mut client_state, &server_state).unwrap();

        // Then
        assert!(client_state.should_kick().is_none());
        assert!(batch.into_stream().next().await.is_none());
    }
}
