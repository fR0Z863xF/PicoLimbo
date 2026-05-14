//! Forge / NeoForge protocol bridge for PicoLimbo.
//!
//! The bridge has three responsibilities, each implemented in its own
//! submodule:
//!
//! 1. **Detect** which Forge dialect a connecting client speaks
//!    (re-exported from `minecraft_packets::handshaking::handshake_packet`).
//! 2. **Record** the Login/Configuration handshake against an upstream
//!    Forge bootstrap server and persist the resulting [`Snapshot`] to disk
//!    (see [`recorder`]).
//! 3. **Replay** that snapshot to incoming Forge clients so they see a
//!    server they recognise (see `replay` — added in later steps).
//!
//! Status-phase `forgeData` pass-through is handled by [`status_proxy`].

pub mod recorder;
pub mod replay;
pub mod snapshot;
pub mod snapshot_io;
pub mod status_proxy;
pub mod upstream_client;
pub mod velocity_forwarder;

// These re-exports are public surface for upcoming modules (recorder,
// replay state machines, status proxy). They are intentionally kept even
// though Step 3 alone does not consume them yet — keeping them here means
// we touch this file once instead of every time a downstream module is
// added.
#[allow(unused_imports)]
pub use snapshot::{Fml2Snapshot, Fml2Step, Fml3Snapshot, Fml3Step, Snapshot, SnapshotError};
