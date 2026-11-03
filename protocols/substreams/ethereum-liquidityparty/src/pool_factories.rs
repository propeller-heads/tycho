use crate::{
    abi,
    params::{encode_addr, Params},
};
use substreams::log;
use substreams_ethereum::{
    pb::eth::v2::{Call, CallType, Log, TransactionTrace},
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
    tx: &TransactionTrace,
) -> Option<ProtocolComponent> {
    if call.address == params.planner {
        if let Some(event) = abi::party_planner::events::PartyStarted::match_and_decode(_log) {
            log::info!("PartyStarted event detected for pool: 0x{}", hex::encode(&event.pool));
            let mut contracts = vec![
                event.pool.clone(),
                params.extra_impl1.clone(),
                params.extra_impl2.clone(),
                params.info.clone(),
            ];
            // The pool constructor delegatecalls PartyPoolExtraImpl1.init, which CREATEs the
            // immutable BFStore (SSTORE2 data contract). Because init runs via delegatecall, the
            // CREATE's caller is the pool itself. The BFStore address is never emitted in an event,
            // so recover it from the transaction trace and index it alongside the pool.
            contracts.extend(
                tx.calls
                    .iter()
                    .filter(|c| c.call_type() == CallType::Create && c.caller == event.pool)
                    .map(|c| c.address.clone()),
            );
            return Some(
                ProtocolComponent::new(&encode_addr(&event.pool))
                    .with_tokens(&event.tokens.clone())
                    .with_contracts(&contracts)
                    .as_swap_type("liquidityparty_pool", ImplementationType::Vm),
            );
        }
    }
    None
}
