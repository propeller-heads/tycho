use ethabi::{Address, FixedBytes, Token};
use tiny_keccak::{Hasher, Keccak};

pub struct PoolConfig {
    pub fee: u64,
    pub tick_spacing: u32,
    pub extension: Address,
}

impl From<[u8; 32]> for PoolConfig {
    fn from(value: [u8; 32]) -> Self {
        Self {
            tick_spacing: u32::from_be_bytes(value[28..32].try_into().unwrap()),
            fee: u64::from_be_bytes(value[20..28].try_into().unwrap()),
            extension: <[u8; 20]>::try_from(&value[0..20])
                .unwrap()
                .into(),
        }
    }
}

impl From<PoolConfig> for FixedBytes {
    fn from(value: PoolConfig) -> Self {
        [
            value
                .extension
                .to_fixed_bytes()
                .as_slice(),
            &value.fee.to_be_bytes(),
            &value.tick_spacing.to_be_bytes(),
        ]
        .concat()
    }
}

pub struct PoolKey {
    pub token0: Address,
    pub token1: Address,
    pub config: PoolConfig,
}

impl PoolKey {
    pub fn into_pool_id(self) -> Vec<u8> {
        let mut hasher = Keccak::v256();

        hasher.update(&ethabi::encode(&[
            Token::Address(self.token0),
            Token::Address(self.token1),
            Token::FixedBytes(self.config.into()),
        ]));

        let mut output = vec![0; 32];
        hasher.finalize(&mut output);
        output
    }
}

#[cfg(test)]
mod tests {
    use ethabi::ethereum_types::H160;
    use substreams_helper::hex::Hexable;

    use super::*;

    #[test]
    fn test_pool_id_computation() {
        let pool_key = PoolKey {
            token0: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48" // USDC
                .parse()
                .unwrap(),
            token1: "0xdAC17F958D2ee523a2206206994597C13D831ec7" // USDT
                .parse()
                .unwrap(),
            config: PoolConfig { fee: 922337203685477, tick_spacing: 100, extension: H160::zero() },
        };

        // https://etherscan.io/tx/0x8d8c4aaee4cc5a23670f7b4894eb63f2eba82779b691e3a97bb073ae857d82e2#eventlog#153
        assert_eq!(
            pool_key.into_pool_id().to_hex(),
            "0x91ffc128bf8e0afbd2c0f14722e2fd5b6625341a5e5f551aa36242d98756798d"
        );
    }
}
