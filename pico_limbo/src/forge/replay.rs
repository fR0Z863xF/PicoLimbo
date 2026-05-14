//! Replay state machines for Forge / `NeoForge` handshakes.
//!
//! The recorder (`recorder.rs`) captures the *server→client* plugin
//! message sequence from an upstream Forge backend. This module is the
//! mirror image: it takes that recorded sequence and pushes it back at
//! a real Forge client connecting to `PicoLimbo`, so the client sees
//! responses indistinguishable from the upstream's.
//!
//! Only one dialect is implemented for now: **FML2** (Minecraft
//! 1.13-1.20.1 wire protocol, handshake in the Login phase). The state
//! machine for FML3 (Configuration phase) will land in a follow-up
//! step.
//!
//! # FML2 replay protocol
//!
//! 1. Client sends `LoginStart`.
//! 2. `PicoLimbo` emits the first recorded snapshot step as a clientbound
//!    `LoginPluginRequest` (`CustomQueryPacket`) — a fresh
//!    `message_id` is allocated and stored in the session's `pending`
//!    map so the inbound response can be matched.
//! 3. Client replies with `LoginPluginResponse` carrying the same
//!    `message_id`.
//! 4. `PicoLimbo` looks up the `message_id` in `pending`, removes it, and
//!    either:
//!    * sends the next recorded step (steps remain), or
//!    * fires `LoginSuccess` and transitions the connection into the
//!      Configuration / Play state (handshake complete).
//!
//! The session is owned by `ClientState` and intentionally Clone-able
//! so the same value can flow through `PacketHandler` return values.

use std::collections::HashMap;

use crate::forge::snapshot::{Fml2Snapshot, Fml3Snapshot};
use minecraft_packets::handshaking::handshake_packet::ForgeKind;

/// Per-connection state for a Forge FML2 handshake replay.
#[derive(Debug, Clone, Default)]
pub struct Fml2ReplaySession {
    /// Index of the snapshot step the *next* outbound packet will
    /// carry. Advanced after every push.
    next_step: usize,

    /// Counter used to mint fresh `message_id`s for outbound LPRs.
    next_message_id: i32,

    /// Maps an outbound `message_id` to the snapshot step index it
    /// represents. Used to detect Forge replies (vs. e.g. Velocity
    /// modern-forwarding replies, which use a different ID range
    /// allocated by `set_velocity_login_message_id`).
    pending: HashMap<i32, usize>,
}

impl Fml2ReplaySession {
    /// Creates an empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if every recorded step has been delivered to the
    /// client and acknowledged. When `true`, the caller should fire
    /// `LoginSuccess` and move the connection to the next state.
    pub fn is_complete(&self, snapshot: &Fml2Snapshot) -> bool {
        self.next_step >= snapshot.steps.len() && self.pending.is_empty()
    }

    /// Allocates a fresh `message_id`, records it as pending against
    /// the next step, and returns `(message_id, step)`. Returns `None`
    /// when there is no more recorded data to send.
    pub fn take_next_step<'a>(
        &mut self,
        snapshot: &'a Fml2Snapshot,
    ) -> Option<(i32, &'a crate::forge::snapshot::Fml2Step)> {
        let step_idx = self.next_step;
        let step = snapshot.steps.get(step_idx)?;
        // We start at message_id = 1 because the existing Velocity
        // path uses a random `i32` token allocated by
        // `rand::Rng::random`, and the chances of collision with our
        // small monotonic counter are astronomically low. Starting at
        // 1 (not 0) leaves room to grow.
        self.next_message_id += 1;
        let id = self.next_message_id;
        self.pending.insert(id, step_idx);
        self.next_step += 1;
        Some((id, step))
    }

    /// Looks up an inbound `message_id` against the pending map.
    /// Returns the snapshot step index it corresponds to and removes
    /// it from the map. Returns `None` if the id was not minted by
    /// this session (likely a Velocity Modern Forwarding reply, which
    /// the existing handler will deal with).
    pub fn consume_response(&mut self, message_id: i32) -> Option<usize> {
        self.pending.remove(&message_id)
    }

    /// True when the session has at least one outstanding outbound
    /// request that the client has not yet replied to.
    #[allow(dead_code)] // Reserved for future timeout handling.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Returns `true` when a client carrying the given [`ForgeKind`] should
/// trigger a replay instead of the vanilla `LoginStart` fast path.
///
/// Currently only FML2 is supported in replay; FML3 follows in a later
/// step. Vanilla / unsupported clients always take the fast path.
pub const fn should_replay(kind: ForgeKind) -> bool {
    matches!(kind, ForgeKind::Fml2 | ForgeKind::Fml3)
}

/// Per-connection state for a Forge FML3 (Configuration-phase)
/// handshake replay.
///
/// Unlike FML2, configuration plugin messages do not carry a
/// `message_id` — the dialog is strictly sequential. The state
/// machine therefore boils down to a single cursor: `next_step` tells
/// us which recorded packet to push next, and each client response on
/// the `fml:handshake` (or `neoforge:handshake`) channel advances it
/// by one.
#[derive(Debug, Clone, Default)]
pub struct Fml3ReplaySession {
    /// Index of the snapshot step the next outbound packet will
    /// carry. `0` at the start of the session.
    next_step: usize,
}

impl Fml3ReplaySession {
    /// Creates an empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the next step to send (if any) and advances the cursor.
    /// Returns `None` when the snapshot is exhausted.
    pub fn take_next_step<'a>(
        &mut self,
        snapshot: &'a Fml3Snapshot,
    ) -> Option<&'a crate::forge::snapshot::Fml3Step> {
        let step = snapshot.steps.get(self.next_step)?;
        self.next_step += 1;
        Some(step)
    }

    /// `true` when every recorded step has been pushed to the client.
    pub const fn is_complete(&self, snapshot: &Fml3Snapshot) -> bool {
        self.next_step >= snapshot.steps.len()
    }

    /// Total number of steps already delivered.
    #[allow(dead_code)] // Reserved for diagnostics.
    pub const fn steps_delivered(&self) -> usize {
        self.next_step
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::snapshot::{Fml2Snapshot, Fml2Step};

    fn snap(n: usize) -> Fml2Snapshot {
        Fml2Snapshot {
            steps: (0..n)
                .map(|i| Fml2Step {
                    channel: format!("fml:loginwrapper#{i}"),
                    payload: vec![u8::try_from(i).expect("test step count fits in u8")],
                })
                .collect(),
        }
    }

    #[test]
    fn new_session_starts_empty_and_incomplete() {
        let s = Fml2ReplaySession::new();
        assert!(!s.has_pending());
        assert!(s.is_complete(&snap(0)));
        assert!(!s.is_complete(&snap(1)));
    }

    #[test]
    fn take_next_step_advances_and_records_pending() {
        let snap = snap(3);
        let mut s = Fml2ReplaySession::new();

        let (id_a, step_a) = s.take_next_step(&snap).unwrap();
        assert_eq!(step_a.channel, "fml:loginwrapper#0");
        assert_eq!(step_a.payload, vec![0]);
        assert!(s.has_pending());
        assert_eq!(s.pending.get(&id_a), Some(&0));

        let (id_b, step_b) = s.take_next_step(&snap).unwrap();
        assert_ne!(id_a, id_b, "message ids must be unique within a session");
        assert_eq!(step_b.channel, "fml:loginwrapper#1");
        assert_eq!(s.pending.len(), 2);
    }

    #[test]
    fn take_next_step_returns_none_at_end() {
        let snap = snap(1);
        let mut s = Fml2ReplaySession::new();
        let _ = s.take_next_step(&snap).unwrap();
        assert!(s.take_next_step(&snap).is_none());
    }

    #[test]
    fn consume_response_removes_from_pending() {
        let snap = snap(2);
        let mut s = Fml2ReplaySession::new();
        let (id, _) = s.take_next_step(&snap).unwrap();
        let idx = s.consume_response(id).unwrap();
        assert_eq!(idx, 0);
        assert!(!s.has_pending());
        // Consuming the same id twice yields None.
        assert!(s.consume_response(id).is_none());
    }

    #[test]
    fn consume_response_ignores_unknown_ids() {
        let mut s = Fml2ReplaySession::new();
        assert!(s.consume_response(12345).is_none());
    }

    #[test]
    fn is_complete_requires_both_advance_and_drained_pending() {
        let snap = snap(2);
        let mut s = Fml2ReplaySession::new();

        let (id_0, _) = s.take_next_step(&snap).unwrap();
        let (id_1, _) = s.take_next_step(&snap).unwrap();
        // All steps queued but client has not replied to any yet.
        assert!(!s.is_complete(&snap));

        s.consume_response(id_0);
        // One reply outstanding.
        assert!(!s.is_complete(&snap));

        s.consume_response(id_1);
        // Now complete.
        assert!(s.is_complete(&snap));
    }

    #[test]
    fn should_replay_matches_supported_dialects() {
        assert!(should_replay(ForgeKind::Fml2));
        assert!(should_replay(ForgeKind::Fml3));
        assert!(!should_replay(ForgeKind::Fml1));
        assert!(!should_replay(ForgeKind::None));
    }
}
