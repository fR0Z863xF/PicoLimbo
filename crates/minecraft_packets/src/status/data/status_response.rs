use pico_text_component::prelude::Component;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
pub struct Version {
    pub name: String,
    pub protocol: i32,
}

#[derive(Serialize, Deserialize)]
pub struct PlayerSample {
    pub name: String,
    pub id: String,
}

#[derive(Serialize, Deserialize)]
pub struct Players {
    pub max: u32,
    pub online: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<Vec<PlayerSample>>,
}

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: Version,
    pub players: Players,
    pub description: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(
        alias = "enforcesSecureChat",
        default = "get_default_enforces_secure_chat"
    )]
    pub enforces_secure_chat: bool,

    /// Forge / NeoForge protocol bridge payload, opaque to PicoLimbo —
    /// when present, advertises this server as Forge-compatible to the
    /// vanilla / Forge client browsing the server list. Vanilla servers
    /// omit this entirely (the field is `skip_serializing_if` to avoid
    /// emitting `"forgeData": null`).
    #[serde(rename = "forgeData", skip_serializing_if = "Option::is_none", default)]
    pub forge_data: Option<Value>,
}

fn get_default_enforces_secure_chat() -> bool {
    false
}

impl StatusResponse {
    pub fn new(
        version_name: String,
        version_protocol: i32,
        description: &Component,
        online_players: u32,
        max_players: u32,
        favicon: Option<String>,
    ) -> Self {
        let description = serde_json::to_value(description).unwrap();
        StatusResponse {
            version: Version {
                name: version_name,
                protocol: version_protocol,
            },
            players: Players {
                max: max_players,
                online: online_players,
                sample: None,
            },
            description,
            favicon,
            enforces_secure_chat: false,
            forge_data: None,
        }
    }

    /// Attaches a Forge / NeoForge `forgeData` payload. Pass `None` to
    /// keep the response vanilla.
    pub fn with_forge_data(mut self, forge_data: Option<Value>) -> Self {
        self.forge_data = forge_data;
        self
    }
}
