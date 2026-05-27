pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use store_protocol_components::store_protocol_components;

mod config {
    use anyhow::{anyhow, Result};

    use crate::lunarbase;

    const LIVE_POOL: lunarbase::Address = [
        0x00, 0x00, 0xef, 0xc4, 0xec, 0x03, 0xa7, 0xc4, 0x7d, 0x3a, 0x38, 0xa9, 0xbe, 0x7f, 0xf1,
        0xd5, 0x2d, 0xd0, 0x1b, 0x99,
    ];
    const NATIVE_ETH_SENTINEL: lunarbase::Address = [0u8; 20];
    const BASE_USDC: lunarbase::Address = [
        0x83, 0x35, 0x89, 0xfc, 0xd6, 0xed, 0xb6, 0xe0, 0x8f, 0x4c, 0x7c, 0x32, 0xd4, 0xf7, 0x1b,
        0x54, 0xbd, 0xa0, 0x29, 0x13,
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Config {
        pub pool: lunarbase::Address,
        pub token_x: lunarbase::Address,
        pub token_y: lunarbase::Address,
        pub tycho_executor: lunarbase::Address,
        pub bootstrap_block: Option<u64>,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                pool: LIVE_POOL,
                token_x: NATIVE_ETH_SENTINEL,
                token_y: BASE_USDC,
                tycho_executor: [0u8; 20],
                bootstrap_block: None,
            }
        }
    }

    impl Config {
        pub fn parse(params: &str) -> Result<Self> {
            let mut config = Self::default();
            for pair in params
                .split('&')
                .filter(|part| !part.is_empty())
            {
                let Some((key, value)) = pair.split_once('=') else {
                    return Err(anyhow!("invalid param pair `{pair}`"));
                };

                match key {
                    "pool" => config.pool = parse_address(value)?,
                    "token_x" => config.token_x = parse_address(value)?,
                    "token_y" => config.token_y = parse_address(value)?,
                    "tycho_executor" => config.tycho_executor = parse_address(value)?,
                    "bootstrap_block" => config.bootstrap_block = Some(value.parse()?),
                    _ => return Err(anyhow!("unknown LunarBase Substreams param `{key}`")),
                }
            }
            Ok(config)
        }
    }

    fn parse_address(value: &str) -> Result<lunarbase::Address> {
        let trimmed = value
            .strip_prefix("0x")
            .unwrap_or(value);
        let decoded = hex::decode(trimmed)?;
        decoded
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("address `{value}` is not 20 bytes"))
    }
}

#[path = "3_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_map_protocol_components.rs"]
mod map_protocol_components;
#[path = "2_store_protocol_components.rs"]
mod store_protocol_components;
