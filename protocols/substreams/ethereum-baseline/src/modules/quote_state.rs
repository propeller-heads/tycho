use crate::abi::b_controller::events::{CreatorFeePctSet, DeployerSet, LiquidityFeePctSet};
use substreams_ethereum::{pb::eth, Event};

pub(crate) fn maybe_update_component_id(log: &eth::v2::Log) -> Option<String> {
    CreatorFeePctSet::match_and_decode(log)
        .map(|event| event.b_token)
        .or_else(|| LiquidityFeePctSet::match_and_decode(log).map(|event| event.b_token))
        .or_else(|| DeployerSet::match_and_decode(log).map(|event| event.b_token))
        .map(|b_token| format!("0x{}", hex::encode(b_token)))
}
