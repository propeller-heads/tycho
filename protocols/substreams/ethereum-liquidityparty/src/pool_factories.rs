use crate::{
    abi,
    params::{encode_addr, Params},
};
use substreams::log;
use substreams_ethereum::{
    pb::eth::v2::{Call, Log, TransactionTrace},
    Event,
};
use tycho_substreams::models::{ImplementationType, ProtocolComponent};

/// Potentially constructs a new ProtocolComponent given a call
///
/// This method is given each individual call within a transaction, the corresponding
/// logs emitted during that call as well as the full transaction trace.
///
/// If this call creates a component in your protocol please construct and return it
/// here. Otherwise, simply return None.
pub fn maybe_create_component(
    params: &Params,
    call: &Call,
    _log: &Log,
    _tx: &TransactionTrace,
) -> Option<ProtocolComponent> {
    if call.address == params.planner {
        if let Some(event) = abi::party_planner::events::PartyStarted::match_and_decode(_log) {
            log::info!("PartyStarted event detected for pool: 0x{}", hex::encode(&event.pool));
            return Some(
                ProtocolComponent::new(&encode_addr(&event.pool))
                    .with_tokens(&event.tokens.clone())
                    .with_contracts(&[
                        event.pool.clone(),
                        params.extra_impl1.clone(),
                        params.extra_impl2.clone(),
                        params.info.clone(),
                    ])
                    .as_swap_type("liquidityparty_pool", ImplementationType::Vm),
            );
        }
    }
    None
}
