pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use store_protocol_components::store_protocol_components;
pub use store_watched::store_watched;

pub(crate) mod config {
    use anyhow::{anyhow, Result};

    use crate::biconomy::Address;

    /// `venue=0x..&executor=0x..&tokens=0x..,0x..&bootstrap_block=N`: the venue, the executor
    /// it fills through, the tokens of its pairs, and the block the component is created at
    /// (the venue's deployment).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Config {
        pub venue: Address,
        pub executor: Address,
        pub tokens: Vec<Address>,
        pub bootstrap_block: u64,
    }

    impl Config {
        pub fn parse(params: &str) -> Result<Self> {
            let mut venue = None;
            let mut executor = None;
            let mut tokens = Vec::new();
            let mut bootstrap_block = None;
            for pair in params
                .split('&')
                .filter(|part| !part.is_empty())
            {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid param pair `{pair}`"))?;
                match key {
                    "venue" => venue = Some(parse_address(value)?),
                    "executor" => executor = Some(parse_address(value)?),
                    "tokens" => {
                        tokens = value
                            .split(',')
                            .filter(|token| !token.is_empty())
                            .map(parse_address)
                            .collect::<Result<_>>()?
                    }
                    "bootstrap_block" => bootstrap_block = Some(value.parse()?),
                    _ => return Err(anyhow!("unknown Biconomy Substreams param `{key}`")),
                }
            }
            if tokens.len() < 2 {
                return Err(anyhow!("`tokens` must list at least two tokens"));
            }
            Ok(Self {
                venue: venue.ok_or_else(|| anyhow!("missing `venue` param"))?,
                executor: executor.ok_or_else(|| anyhow!("missing `executor` param"))?,
                tokens,
                bootstrap_block: bootstrap_block
                    .ok_or_else(|| anyhow!("missing `bootstrap_block` param"))?,
            })
        }

        pub fn component_id(&self) -> String {
            crate::biconomy::hex_address(&self.venue)
        }
    }

    fn parse_address(value: &str) -> Result<Address> {
        hex::decode(value.trim_start_matches("0x"))?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("address `{value}` is not 20 bytes"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_params() {
            let config = Config::parse(
                "venue=0x0000000000000000000000000000000000000001&\
                 executor=0x0000000000000000000000000000000000000002&\
                 tokens=0x0000000000000000000000000000000000000003,0x0000000000000000000000000000000000000004&\
                 bootstrap_block=10",
            )
            .unwrap();

            assert_eq!(config.venue[19], 1);
            assert_eq!(config.executor[19], 2);
            assert_eq!(config.tokens.len(), 2);
            assert_eq!(config.bootstrap_block, 10);
            assert_eq!(config.component_id(), "0x0000000000000000000000000000000000000001");
        }

        #[test]
        fn rejects_missing_params() {
            assert!(Config::parse("venue=0x0000000000000000000000000000000000000001").is_err());
        }
    }
}

#[path = "3_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_map_protocol_components.rs"]
mod map_protocol_components;
#[path = "2_store_protocol_components.rs"]
mod store_protocol_components;
#[path = "2_store_watched.rs"]
mod store_watched;
