use crate::{
    abi::{b_factory::events::PoolCreated, b_swap::events::Swap},
    pool_factories::DeploymentConfig,
};
use substreams::{prelude::*, store::StoreGetString};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::prelude::BalanceDelta;

pub(crate) fn extract_balance_deltas(
    config: &DeploymentConfig,
    tx: &eth::v2::TransactionTrace,
    log: &eth::v2::Log,
    reserve_store: &StoreGetString,
) -> Vec<BalanceDelta> {
    if log.address != config.relay_address {
        return vec![];
    }

    if let Some(event) = PoolCreated::match_and_decode(log) {
        let component_id = format!("0x{}", hex::encode(&event.b_token_address)).into_bytes();

        return vec![
            BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx.into()),
                token: event.b_token_address,
                delta: event
                    .total_b_tokens
                    .to_signed_bytes_be(),
                component_id: component_id.clone(),
            },
            BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx.into()),
                token: event.reserve_address,
                delta: event
                    .total_reserves
                    .to_signed_bytes_be(),
                component_id,
            },
        ];
    }

    if let Some(event) = Swap::match_and_decode(log) {
        let component_id = format!("0x{}", hex::encode(&event.b_token));
        let Some(reserve) = reserve_store.get_last(format!("reserve:{component_id}")) else {
            return vec![];
        };
        let Ok(reserve) = hex::decode(reserve) else {
            return vec![];
        };

        let b_token_delta = event.b_token_delta.neg();
        let reserve_delta = event.reserve_delta.neg() - event.total_fee + event.liquidity_fee;
        let component_id = component_id.into_bytes();

        return vec![
            BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx.into()),
                token: event.b_token,
                delta: b_token_delta.to_signed_bytes_be(),
                component_id: component_id.clone(),
            },
            BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx.into()),
                token: reserve,
                delta: reserve_delta.to_signed_bytes_be(),
                component_id,
            },
        ];
    }

    vec![]
}
