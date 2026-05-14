use minecraft_protocol::prelude::*;

#[derive(PacketOut)]
pub struct ConfigurationClientBoundPluginMessagePacket {
    channel: Identifier,
    data: LengthPaddedVec<i8>,
}

impl ConfigurationClientBoundPluginMessagePacket {
    pub fn brand(brand: impl ToString) -> Self {
        Self {
            channel: Identifier::vanilla_unchecked("brand"),
            data: LengthPaddedVec::new(
                brand
                    .to_string()
                    .as_bytes()
                    .iter()
                    .map(|&b| b as i8)
                    .collect::<Vec<_>>(),
            ),
        }
    }

    /// Constructs a Configuration plugin message from raw channel and
    /// payload bytes. Used by the Forge replay state machine to
    /// forward verbatim recorded server→client packets.
    pub fn raw(channel: Identifier, data: Vec<i8>) -> Self {
        Self {
            channel,
            data: LengthPaddedVec::new(data),
        }
    }
}
