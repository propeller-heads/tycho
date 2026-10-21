use crate::{
    events::{BalanceEventTrait, Trade},
    pb::cowamm::CowPool,
};
use substreams_ethereum::pb::eth::v2::Log;
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for Trade {
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        pool: &CowPool,
        event: &Log,
    ) -> Vec<BalanceDelta> {
        //if sell token is token_a is true then buy token is token_b is automatically true
        //we create deltas for both changes from one Trade event

        //no more than conditions can ever be true at the same time
        let mut changed = Vec::new();
        let tx_clone = Some(tx.clone());
        let component_id = pool
            .address
            .to_hex()
            .as_bytes()
            .to_vec();

        // Precompute deltas
        let sell_delta = self
            .sell_amount
            .clone()
            .neg()
            .to_signed_bytes_be();
        let buy_delta = self
            .buy_amount
            .clone()
            .to_signed_bytes_be();

        // Helper closure to reduce repetition
        let mut push_delta = |token: &Vec<u8>, delta: Vec<u8>| {
            changed.push(BalanceDelta {
                ord: event.ordinal,
                tx: tx_clone.clone(),
                token: token.clone(),
                delta,
                component_id: component_id.clone(),
            });
        };

        // Sells (negative delta)
        if self.sell_token == pool.token_a {
            push_delta(&pool.token_a, sell_delta.clone());
        } else if self.sell_token == pool.token_b {
            push_delta(&pool.token_b, sell_delta.clone());
        }

        // Buys (positive delta)
        if self.buy_token == pool.token_a {
            push_delta(&pool.token_a, buy_delta.clone());
        } else if self.buy_token == pool.token_b {
            push_delta(&pool.token_b, buy_delta);
        }

        changed
    }
}
