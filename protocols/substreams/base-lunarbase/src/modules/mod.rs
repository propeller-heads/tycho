pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use store_protocol_components::store_protocol_components;

mod config {
    use anyhow::{anyhow, Result};
    use ethabi::ethereum_types::U256;

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
    pub struct PoolConfig {
        pub pool: lunarbase::Address,
        pub token_x: lunarbase::Address,
        pub token_y: lunarbase::Address,
        pub bootstrap_block: Option<u64>,
        pub bootstrap_state: lunarbase::BootstrapState,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Config {
        pub pools: Vec<PoolConfig>,
        pub tycho_executor: lunarbase::Address,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                pools: vec![PoolConfig {
                    pool: LIVE_POOL,
                    token_x: NATIVE_ETH_SENTINEL,
                    token_y: BASE_USDC,
                    bootstrap_block: None,
                    bootstrap_state: lunarbase::BootstrapState::default(),
                }],
                tycho_executor: [0u8; 20],
            }
        }
    }

    impl Config {
        pub fn parse(params: &str) -> Result<Self> {
            let mut config = Self::default();
            let mut single_pool = config.pools[0];
            for pair in params
                .split('&')
                .filter(|part| !part.is_empty())
            {
                let Some((key, value)) = pair.split_once('=') else {
                    return Err(anyhow!("invalid param pair `{pair}`"));
                };

                match key {
                    "pool" => single_pool.pool = parse_address(value)?,
                    "token_x" => single_pool.token_x = parse_address(value)?,
                    "token_y" => single_pool.token_y = parse_address(value)?,
                    "tycho_executor" => config.tycho_executor = parse_address(value)?,
                    "bootstrap_block" => single_pool.bootstrap_block = Some(value.parse()?),
                    "blacklist_fee_multiplier" => {
                        single_pool
                            .bootstrap_state
                            .blacklist_fee_multiplier = parse_u256(value)?
                    }
                    "pools" => config.pools = parse_pools(value)?,
                    _ => return Err(anyhow!("unknown LunarBase Substreams param `{key}`")),
                }
            }
            if params
                .split('&')
                .filter_map(|part| part.split_once('='))
                .all(|(key, _)| key != "pools")
            {
                config.pools = vec![single_pool];
            }
            Ok(config)
        }
    }

    impl PoolConfig {
        pub fn component_id(&self) -> String {
            lunarbase::component_id(self.pool)
        }
    }

    fn parse_pools(value: &str) -> Result<Vec<PoolConfig>> {
        let pools = value
            .split(',')
            .filter(|pool| !pool.is_empty())
            .map(parse_pool)
            .collect::<Result<Vec<_>>>()?;
        if pools.is_empty() {
            return Err(anyhow!("LunarBase `pools` param must contain at least one pool"));
        }
        Ok(pools)
    }

    fn parse_pool(value: &str) -> Result<PoolConfig> {
        let mut parts = value.split(':');
        let pool = parts
            .next()
            .ok_or_else(|| anyhow!("missing pool address in `{value}`"))
            .and_then(parse_address)?;
        let token_x = parts
            .next()
            .ok_or_else(|| anyhow!("missing token_x address in `{value}`"))
            .and_then(parse_address)?;
        let token_y = parts
            .next()
            .ok_or_else(|| anyhow!("missing token_y address in `{value}`"))
            .and_then(parse_address)?;
        let bootstrap_block = parts
            .next()
            .map(str::parse)
            .transpose()?;
        let mut bootstrap_state = lunarbase::BootstrapState::default();
        if let Some(multiplier) = parts.next() {
            bootstrap_state.blacklist_fee_multiplier = parse_u256(multiplier)?;
        }
        if parts.next().is_some() {
            return Err(anyhow!("invalid LunarBase pool tuple `{value}`"));
        }
        Ok(PoolConfig { pool, token_x, token_y, bootstrap_block, bootstrap_state })
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

    fn parse_u256(value: &str) -> Result<U256> {
        U256::from_dec_str(value).map_err(|_| anyhow!("invalid uint256 `{value}`"))
    }
}

#[path = "3_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_map_protocol_components.rs"]
mod map_protocol_components;
#[path = "2_store_protocol_components.rs"]
mod store_protocol_components;

#[cfg(test)]
mod tests {
    use super::config::Config;

    #[test]
    fn parses_single_pool_config() {
        let config = Config::parse(
            "pool=0x0000000000000000000000000000000000000001&\
             token_x=0x0000000000000000000000000000000000000002&\
             token_y=0x0000000000000000000000000000000000000003&\
             bootstrap_block=10&\
             blacklist_fee_multiplier=100",
        )
        .expect("valid config");

        assert_eq!(config.pools.len(), 1);
        assert_eq!(config.pools[0].pool, address(1));
        assert_eq!(config.pools[0].token_x, address(2));
        assert_eq!(config.pools[0].token_y, address(3));
        assert_eq!(config.pools[0].bootstrap_block, Some(10));
        assert_eq!(
            config.pools[0]
                .bootstrap_state
                .blacklist_fee_multiplier,
            100.into()
        );
    }

    #[test]
    fn parses_multi_pool_config() {
        let config = Config::parse(
            "pools=\
             0x0000000000000000000000000000000000000001:\
             0x0000000000000000000000000000000000000002:\
             0x0000000000000000000000000000000000000003:10:100,\
             0x0000000000000000000000000000000000000004:\
             0x0000000000000000000000000000000000000005:\
             0x0000000000000000000000000000000000000006:20",
        )
        .expect("valid config");

        assert_eq!(config.pools.len(), 2);
        assert_eq!(config.pools[0].pool, address(1));
        assert_eq!(config.pools[0].bootstrap_block, Some(10));
        assert_eq!(
            config.pools[0]
                .bootstrap_state
                .blacklist_fee_multiplier,
            100.into()
        );
        assert_eq!(config.pools[1].pool, address(4));
        assert_eq!(config.pools[1].token_x, address(5));
        assert_eq!(config.pools[1].token_y, address(6));
        assert_eq!(config.pools[1].bootstrap_block, Some(20));
        assert_eq!(
            config.pools[1]
                .bootstrap_state
                .blacklist_fee_multiplier,
            1.into()
        );
    }

    fn address(last_byte: u8) -> [u8; 20] {
        let mut address = [0u8; 20];
        address[19] = last_byte;
        address
    }
}
