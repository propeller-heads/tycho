use substreams::scalar::BigInt;

use crate::pool_key::{PoolConfig, PoolKey};

pub type OrderKey = (Vec<u8>, Vec<u8>, BigInt, BigInt, BigInt);

impl PoolKey {
    pub fn from_order_key(key: &OrderKey, twamm_address: &Vec<u8>) -> Self {
        let (token0, token1) = if key.1 > key.0 { (&key.0, &key.1) } else { (&key.1, &key.0) };

        Self {
            token0: <&[u8; 20]>::try_from(token0.as_slice())
                .unwrap()
                .into(),
            token1: <&[u8; 20]>::try_from(token1.as_slice())
                .unwrap()
                .into(),
            config: PoolConfig {
                fee: key.2.to_u64(),
                tick_spacing: 0,
                extension: <&[u8; 20]>::try_from(twamm_address.as_slice())
                    .unwrap()
                    .into(),
            },
        }
    }
}
