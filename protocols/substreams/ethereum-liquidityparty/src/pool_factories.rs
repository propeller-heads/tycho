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

#[cfg(test)]
mod tests {
    use super::*;
    use ethabi::Token;
    use substreams::hex;
    use tycho_substreams::attributes::json_serialize_address_list;

    const PLANNER: [u8; 20] = [0x11; 20];
    const POOL: [u8; 20] = [0x22; 20];

    fn params() -> Params {
        Params {
            planner: PLANNER.to_vec(),
            info: vec![0x33; 20],
            extra_impl1: vec![0x44; 20],
            extra_impl2: vec![0x55; 20],
        }
    }

    fn party_started_log(tokens: &[Vec<u8>]) -> Log {
        let mut pool_topic = vec![0u8; 12];
        pool_topic.extend_from_slice(&POOL);
        Log {
            address: PLANNER.to_vec(),
            topics: vec![
                hex!("f45e30d37dc5ef98e17708308b886213b8f485ea52d495067eed8d4977423f22").to_vec(),
                pool_topic,
            ],
            data: ethabi::encode(&[
                Token::String("name".into()),
                Token::String("symbol".into()),
                Token::Array(
                    tokens
                        .iter()
                        .map(|t| Token::Address(ethabi::Address::from_slice(t)))
                        .collect(),
                ),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn created_component_marks_all_tokens_self_contained() {
        let tokens = vec![vec![0xaa; 20], vec![0xbb; 20], vec![0xcc; 20]];
        let call = Call { address: PLANNER.to_vec(), ..Default::default() };
        let log = party_started_log(&tokens);

        let component =
            maybe_create_component(&params(), &call, &log, &TransactionTrace::default())
                .expect("PartyStarted creates a component");

        let attribute = component
            .static_att
            .iter()
            .find(|a| a.name == "self_contained_tokens")
            .expect("self_contained_tokens attribute is set");
        assert_eq!(attribute.value, json_serialize_address_list(&tokens));
    }
}
