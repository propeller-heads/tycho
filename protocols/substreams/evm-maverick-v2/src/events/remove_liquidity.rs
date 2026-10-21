use crate::{
    abi::pool::events::PoolRemoveLiquidity, events::BalanceEventTrait, pb::maverick::v2::Pool,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for PoolRemoveLiquidity {
    fn get_balance_delta(&self, tx: &Transaction, pool: &Pool, ordinal: u64) -> Vec<BalanceDelta> {
        let changed_balance = vec![
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: pool.token_a.clone(),
                delta: self
                    .token_a_out
                    .neg()
                    .clone()
                    .to_signed_bytes_be(),
                component_id: pool
                    .address
                    .clone()
                    .to_hex()
                    .as_bytes()
                    .to_vec(),
            },
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: pool.token_b.clone(),
                delta: self
                    .token_b_out
                    .neg()
                    .clone()
                    .to_signed_bytes_be(),
                component_id: pool
                    .address
                    .clone()
                    .to_hex()
                    .as_bytes()
                    .to_vec(),
            },
        ];
        changed_balance
    }
}
