use crate::server::client_state::ClientState;
use crate::server::controllable_interval::ControllableInterval;
use minecraft_protocol::prelude::ProtocolVersion;
use net::packet_stream::{PacketStream, PacketStreamError};
use net::raw_packet::RawPacket;
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub struct ClientData {
    client_state: Arc<Mutex<ClientState>>,
    packet_stream: Arc<Mutex<PacketStream<TcpStream>>>,
    keep_alive_interval: Arc<Mutex<ControllableInterval>>,
    action_bar_interval: Arc<Mutex<ControllableInterval>>,
}

impl ClientData {
    pub fn new(socket: TcpStream) -> Self {
        let client_state = ClientState::default();
        let packet_stream = PacketStream::new(socket);
        let keep_alive_interval = ControllableInterval::new();
        let action_bar_interval = ControllableInterval::new();

        Self {
            client_state: Arc::new(Mutex::new(client_state)),
            packet_stream: Arc::new(Mutex::new(packet_stream)),
            keep_alive_interval: Arc::new(Mutex::new(keep_alive_interval)),
            action_bar_interval: Arc::new(Mutex::new(action_bar_interval)),
        }
    }

    // Client state

    #[inline]
    pub async fn client(&self) -> tokio::sync::MutexGuard<'_, ClientState> {
        self.client_state.lock().await
    }

    pub async fn protocol_version(&self) -> ProtocolVersion {
        self.client().await.protocol_version()
    }

    // Stream

    pub async fn stream(&self) -> tokio::sync::MutexGuard<'_, PacketStream<TcpStream>> {
        self.packet_stream.lock().await
    }

    pub async fn write_packet(&self, raw_packet: RawPacket) -> Result<(), PacketStreamError> {
        self.stream().await.write_packet(raw_packet).await
    }

    pub async fn read_packet(&self) -> Result<RawPacket, PacketStreamError> {
        self.stream().await.read_packet().await
    }

    pub async fn shutdown(&self) -> Result<(), PacketStreamError> {
        self.stream().await.get_stream().shutdown().await?;
        self.keep_alive_interval().await.clear_interval().await;
        self.action_bar_interval().await.clear_interval().await;
        Ok(())
    }

    // Keep alive

    pub async fn enable_keep_alive_if_needed(&self) {
        if self.client().await.should_enable_keep_alive() {
            if self
                .protocol_version()
                .await
                .is_before_inclusive(ProtocolVersion::V1_7_6)
            {
                let start = Instant::now().add(Duration::from_secs(2));
                let period = Duration::from_secs(2);
                self.keep_alive_interval().await.set_interval_at(start, period).await;
            } else {
                let period = Duration::from_secs(15);
                self.keep_alive_interval().await.set_interval(period).await;
            }
            self.client().await.set_keep_alive_enabled();
        }
    }

    pub async fn keep_alive_tick(&self) {
        self.keep_alive_interval().await.tick().await;
    }

    #[inline]
    async fn keep_alive_interval(&self) -> tokio::sync::MutexGuard<'_, ControllableInterval> {
        self.keep_alive_interval.lock().await
    }

    // Action bar

    pub async fn enable_action_bar(&self) {
        // The first tick will happen immediately, then every 3 seconds
        let period = Duration::from_secs(3);
        self.action_bar_interval().await.set_interval(period).await;
    }

    pub async fn action_bar_tick(&self) {
        self.action_bar_interval().await.tick().await;
    }

    #[inline]
    async fn action_bar_interval(&self) -> tokio::sync::MutexGuard<'_, ControllableInterval> {
        self.action_bar_interval.lock().await
    }
}
