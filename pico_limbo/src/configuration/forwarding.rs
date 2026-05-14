use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ModernForwardingConfig {
    enabled: bool,
    secret: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct BungeeCordForwardingConfig {
    enabled: bool,
    bungee_guard: bool,
    tokens: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct StructuredForwarding {
    velocity: ModernForwardingConfig,
    bungee_cord: BungeeCordForwardingConfig,
}

impl StructuredForwarding {
    /// Read-only access to the `velocity` sub-config. Used by the
    /// Forge recorder bootstrap path to extract the Modern Forwarding
    /// secret without consuming the structured config.
    pub fn velocity_view(&self) -> &ModernForwardingConfig {
        &self.velocity
    }
}

impl ModernForwardingConfig {
    /// Whether modern forwarding is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The shared HMAC secret. Empty when forwarding is disabled.
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(tag = "method", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaggedForwarding {
    #[default]
    #[serde(alias = "none")]
    None,

    #[serde(alias = "legacy")]
    Legacy,

    #[serde(alias = "bungee_guard")]
    BungeeGuard { tokens: Vec<String> },

    #[serde(alias = "modern")]
    Modern { secret: String },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ForwardingConfig {
    Structured(StructuredForwarding),
    Tagged(TaggedForwarding),
}

impl Default for ForwardingConfig {
    fn default() -> Self {
        Self::Tagged(TaggedForwarding::default())
    }
}

impl From<ForwardingConfig> for TaggedForwarding {
    fn from(cfg: ForwardingConfig) -> Self {
        match cfg {
            ForwardingConfig::Tagged(forwarding) => forwarding,
            ForwardingConfig::Structured(forwarding) => {
                if forwarding.velocity.enabled {
                    Self::Modern {
                        secret: forwarding.velocity.secret,
                    }
                } else if forwarding.bungee_cord.enabled {
                    if forwarding.bungee_cord.bungee_guard {
                        Self::BungeeGuard {
                            tokens: forwarding.bungee_cord.tokens,
                        }
                    } else {
                        Self::Legacy
                    }
                } else {
                    Self::None
                }
            }
        }
    }
}
