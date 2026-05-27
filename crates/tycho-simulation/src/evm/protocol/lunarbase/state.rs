use lunarbase_pmm_math::{PoolParams, U256};

pub type Address = [u8; 20];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseState {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub anchor_price_x96: u128,
    pub fee_ask_x24: u32,
    pub fee_bid_x24: u32,
    pub latest_update_block: u64,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub concentration_k: u32,
    pub block_delay: u64,
    pub paused: bool,
    pub blacklist_fee_multiplier: U256,
    pub executor_whitelisted: bool,
}

impl LunarBaseState {
    pub fn pool_params(&self) -> PoolParams {
        PoolParams {
            sqrt_price_x96: self.anchor_price_x96,
            fee_ask_x24: self.fee_ask_x24,
            fee_bid_x24: self.fee_bid_x24,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            concentration_k: self.concentration_k,
        }
    }

    pub fn is_fresh(&self, block_number: u64) -> bool {
        block_number <
            self.latest_update_block
                .saturating_add(self.block_delay)
    }

    pub fn fee_multiplier(&self) -> U256 {
        if self.executor_whitelisted || self.blacklist_fee_multiplier.is_zero() {
            U256::from(1u64)
        } else {
            self.blacklist_fee_multiplier
        }
    }
}
