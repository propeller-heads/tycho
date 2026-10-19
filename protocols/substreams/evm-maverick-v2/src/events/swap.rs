use crate::{abi::pool::events::PoolSwap, events::BalanceEventTrait, pb::maverick::v2::Pool};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for PoolSwap {
    fn get_balance_delta(&self, tx: &Transaction, pool: &Pool, ordinal: u64) -> Vec<BalanceDelta> {
        let (token_in, token_out, amount_in, amount_out) = if self.params.1 {
            (&pool.token_a, &pool.token_b, &self.amount_in, &self.amount_out)
        } else {
            (&pool.token_b, &pool.token_a, &self.amount_in, &self.amount_out)
        };

        vec![
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: token_in.clone(),
                delta: amount_in.clone().to_signed_bytes_be(),
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
                token: token_out.clone(),
                delta: amount_out
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
        ]
    }
}
