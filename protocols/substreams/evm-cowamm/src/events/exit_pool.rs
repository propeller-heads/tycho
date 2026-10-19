use crate::{abi::b_cow_pool::events::LogExit, events::BalanceEventTrait, pb::cowamm::CowPool};
use substreams_ethereum::pb::eth::v2::Log;
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for LogExit {
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        pool: &CowPool,
        event: &Log,
    ) -> Vec<BalanceDelta> {
        let changed_balance = vec![BalanceDelta {
            ord: event.ordinal,
            tx: Some(tx.clone()),
            token: self.token_out.clone(),
            delta: self
                .token_amount_out
                .neg()
                .clone()
                .to_signed_bytes_be(),
            component_id: pool
                .address
                .clone()
                .to_hex()
                .as_bytes()
                .to_vec(),
        }];
        changed_balance
    }
}
