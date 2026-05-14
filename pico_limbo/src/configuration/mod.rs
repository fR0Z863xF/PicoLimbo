pub mod boss_bar;
pub mod commands;
mod compression;
pub mod config;
mod env_placeholders;
pub mod forge;
pub mod forwarding;
mod game_mode_config;
mod require_boolean;
mod server_list;
pub mod tab_list;
pub mod title;
pub mod world_config;

// `ForgeConfig` is referenced by `Config` directly via the `forge` module —
// the re-export below will be consumed by later steps (recorder, status
// proxy) that pull the config out of `ServerState` without traversing the
// crate's module tree.
#[allow(unused_imports)]
pub use forge::ForgeConfig;
pub use forwarding::TaggedForwarding;
