//! What a `SubsidyPayer` did with each signed subsidy: paid it (`SubsidyPaid`), or skipped it
//! for a reason it names (`SubsidySkipped`), the swap filling either way.
//!
//! An event counts only inside a router swap: the payer runs inside the router call that
//! `map_trades` stores, so the nearest enclosing router call with a swap selector is the trade
//! the subsidy belongs to. That call is also what keeps a contract emitting the same events
//! elsewhere out, so no payer address is configured and a new payer needs no change here. A
//! transaction can hold several router calls, probes included, so the transaction hash alone
//! would not say which one carried the subsidy.
use std::collections::HashMap;

use anyhow::Result;
use substreams::scalar::BigInt;
use substreams_ethereum::{
    pb::eth::v2::{Block, Call, Log},
    Event,
};

use super::block_timestamp;
use crate::{
    abi::subsidy_payer::events::{SubsidyPaid, SubsidySkipped},
    decode::decode_swap_call,
    params::Params,
    pb::tycho::router::v1::{
        subsidy_event::Outcome, SubsidyEvent, SubsidyEvents, SubsidyPayment, SubsidySkip,
    },
};

/// Emits every `SubsidyPaid` and `SubsidySkipped` logged inside a configured router's swap call
/// the chain kept.
#[substreams::handlers::map]
pub fn map_subsidy_events(params: String, block: Block) -> Result<SubsidyEvents> {
    let params = Params::parse(&params)?;
    Ok(SubsidyEvents { events: subsidy_events(&params, &block) })
}

/// Whether the log is a `SubsidyPaid` or `SubsidySkipped`, from any emitter. The block index
/// uses it to keep the blocks `map_subsidy_events` needs.
pub(crate) fn is_subsidy_log(log: &Log) -> bool {
    SubsidyPaid::match_log(log) || SubsidySkipped::match_log(log)
}

/// The subsidy events of one block, read from the calls of successful transactions, and only
/// from calls whose state the chain kept: a payment in a reverted call moved nothing. The call
/// map the trade lookup needs is built only for a transaction that holds such an event.
fn subsidy_events(params: &Params, block: &Block) -> Vec<SubsidyEvent> {
    let timestamp = block_timestamp(block);
    let mut events = Vec::new();
    for tx in block.transactions() {
        let decoded: Vec<(&Call, &Log, Vec<u8>, Outcome)> = tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| {
                call.logs
                    .iter()
                    .map(move |log| (call, log))
            })
            .filter_map(|(call, log)| {
                decode_event(log).map(|(nonce, outcome)| (call, log, nonce, outcome))
            })
            .collect();
        if decoded.is_empty() {
            continue;
        }
        let calls: HashMap<u32, &Call> = tx
            .calls
            .iter()
            .map(|call| (call.index, call))
            .collect();
        for (call, log, nonce, outcome) in decoded {
            let Some(trade_call_index) = enclosing_trade(params, &calls, call) else {
                continue;
            };
            events.push(SubsidyEvent {
                chain: params.chain.clone(),
                block_number: block.number,
                block_timestamp: timestamp,
                tx_hash: tx.hash.clone(),
                log_index: log.index,
                payer: call.address.clone(),
                nonce,
                trade_call_index,
                outcome: Some(outcome),
            });
        }
    }
    events
}

/// The nonce and outcome of a `SubsidyPaid` or `SubsidySkipped` log; `None` for any other log.
fn decode_event(log: &Log) -> Option<(Vec<u8>, Outcome)> {
    if let Some(paid) = SubsidyPaid::match_and_decode(log) {
        let payment = SubsidyPayment {
            bound_to: paid.bound_to,
            receiver: paid.to,
            token: paid.token,
            amount: paid.amount.to_string(),
        };
        return Some((word(&paid.nonce), Outcome::Paid(payment)));
    }
    let skipped = SubsidySkipped::match_and_decode(log)?;
    let skip = SubsidySkip { reason: skip_reason(&skipped.reason) };
    Some((word(&skipped.nonce), Outcome::Skipped(skip)))
}

/// A `uint256` as the 32 bytes it was signed as.
fn word(value: &BigInt) -> Vec<u8> {
    let bytes = value.to_bytes_be().1;
    let mut word = vec![0u8; 32usize.saturating_sub(bytes.len())];
    word.extend_from_slice(&bytes);
    word
}

/// The payer's `SkipReason` enum by value. A value this code does not know, from a newer payer,
/// is kept as its number.
fn skip_reason(reason: &BigInt) -> String {
    match reason.to_string().as_str() {
        "0" => "none",
        "1" => "paused",
        "2" => "expired",
        "3" => "invalid_signature",
        "4" => "caller_mismatch",
        "5" => "below_min_amount_in",
        "6" => "nonce_used",
        "7" => "insufficient_balance",
        "8" => "transfer_failed",
        other => return other.to_string(),
    }
    .to_string()
}

/// The index of the nearest call enclosing `call` that `map_trades` stores as a trade: a call
/// into a configured router's swap entry point. `None` when none does.
///
/// An executor runs by `delegatecall`, so its frame carries the router's address too; the swap
/// selector is what tells the trade from it. The walk takes at most one step per call of the
/// transaction, so a malformed trace cannot loop it.
fn enclosing_trade(params: &Params, calls: &HashMap<u32, &Call>, call: &Call) -> Option<u32> {
    let mut current = call;
    for _ in 0..calls.len() {
        if current.parent_index == 0 {
            return None;
        }
        current = calls.get(&current.parent_index)?;
        let Some(router) = params.router(&current.address) else {
            continue;
        };
        if decode_swap_call(router.version, current).is_some() {
            return Some(current.index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::TransactionTrace;

    use super::*;

    const PAYER: &str = "bfdf58dc941cb83a32afe0fc008288d7ea509e7c";
    const ROUTER: &str = "aba5b53b03eafad1c5fc8bd5fc765fc85bb3de67";
    const OTHER: &str = "1111111111111111111111111111111111111111";
    // keccak256("SubsidyPaid(uint256,address,address,address,uint256)")
    const PAID: &str = "a3c020fea41365130e645110c836a51e4a3cc693bc032223d7d0a737df76ed8e";
    // keccak256("SubsidySkipped(uint256,uint8)")
    const SKIPPED: &str = "b1e3823a20c3731a75cfe9552adcc003c6cf790cf9e9eada2e7f5edcaead934e";
    const USDC: &str = "833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const NONCE: &str = "5af733b729a04609bad2d32a47af0650aa21d622636702387c63baaf875ef41c";

    /// The selector of `singleSwap` on a v3_1 router, from its ABI.
    fn single_swap_v3_1() -> String {
        let abi = include_str!("../../abi/TychoRouterV3_1.json");
        let contract = ethabi::Contract::load(abi.as_bytes()).expect("valid router ABI");
        hex::encode(
            contract
                .function("singleSwap")
                .expect("singleSwap")
                .short_signature(),
        )
    }

    fn params() -> Params {
        Params::parse(&format!("chain=base&routers=0x{ROUTER}:v3_1")).unwrap()
    }

    fn word_of(hex_tail: &str) -> Vec<u8> {
        hex::decode(format!("{:0>64}", hex_tail)).unwrap()
    }

    /// The `SubsidyPaid` the base payer logged in transaction 0xf344…495a, block 53844164.
    fn paid_log(index: u32) -> Log {
        Log {
            address: hex::decode(PAYER).unwrap(),
            topics: vec![
                word_of(PAID),
                word_of(NONCE),
                word_of("b8df65dd1ff912bc3159c3d74ea134cc8da56aa5"),
                word_of(USDC),
            ],
            data: hex::decode(format!("{:0>64}{:0>64}", ROUTER, "36a")).unwrap(),
            index,
            ..Default::default()
        }
    }

    fn skipped_log(reason: u8) -> Log {
        Log {
            address: hex::decode(PAYER).unwrap(),
            topics: vec![word_of(SKIPPED), word_of(NONCE)],
            data: word_of(&format!("{reason:x}")),
            index: 3,
            ..Default::default()
        }
    }

    fn call(index: u32, parent_index: u32, address: &str, input: &str, logs: Vec<Log>) -> Call {
        Call {
            index,
            parent_index,
            address: hex::decode(address).unwrap(),
            input: hex::decode(input).unwrap(),
            logs,
            ..Default::default()
        }
    }

    /// A transaction in which the router's swap call (index 2) runs an executor by delegatecall
    /// (index 3, the router's address again), which calls the payer (index 4).
    fn calls_through_the_router(payer_logs: Vec<Log>) -> Vec<Call> {
        vec![
            call(1, 0, OTHER, "", vec![]),
            call(2, 1, ROUTER, &single_swap_v3_1(), vec![]),
            call(3, 2, ROUTER, "deadbeef", vec![]),
            call(4, 3, PAYER, "", payer_logs),
        ]
    }

    fn block(status: i32, calls: Vec<Call>) -> Block {
        Block {
            number: 53844164,
            transaction_traces: vec![TransactionTrace {
                status,
                hash: vec![0xf3; 32],
                calls,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn payment(event: &SubsidyEvent) -> &SubsidyPayment {
        match &event.outcome {
            Some(Outcome::Paid(payment)) => payment,
            other => panic!("not a payment: {other:?}"),
        }
    }

    fn skip_reason_of(event: &SubsidyEvent) -> &str {
        match &event.outcome {
            Some(Outcome::Skipped(skip)) => &skip.reason,
            other => panic!("not a skip: {other:?}"),
        }
    }

    #[test]
    fn a_payment_is_decoded_and_joined_to_its_trade() {
        let block = block(1, calls_through_the_router(vec![paid_log(29)]));

        let events = subsidy_events(&params(), &block);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        let paid = payment(event);
        assert_eq!(hex::encode(&event.nonce), NONCE);
        assert_eq!(hex::encode(&paid.bound_to), "b8df65dd1ff912bc3159c3d74ea134cc8da56aa5");
        assert_eq!(hex::encode(&paid.receiver), ROUTER, "paid to the router, which pays the user");
        assert_eq!(hex::encode(&paid.token), USDC);
        assert_eq!(paid.amount, "874");
        assert_eq!(event.log_index, 29);
        assert_eq!(event.block_number, 53844164);
        assert_eq!(hex::encode(&event.payer), PAYER, "the emitter");
        assert_eq!(event.trade_call_index, 2, "the swap call, not the executor's frame");
    }

    #[test]
    fn a_skip_carries_its_reason() {
        let logs = vec![skipped_log(7), skipped_log(9)];

        let events = subsidy_events(&params(), &block(1, calls_through_the_router(logs)));

        let reasons: Vec<&str> = events
            .iter()
            .map(skip_reason_of)
            .collect();
        assert_eq!(reasons, ["insufficient_balance", "9"]);
        assert_eq!(hex::encode(&events[0].nonce), NONCE);
    }

    #[test]
    fn an_event_outside_a_router_swap_is_ignored() {
        let calls = vec![call(1, 0, OTHER, "", vec![]), call(2, 1, PAYER, "", vec![paid_log(5)])];

        assert!(subsidy_events(&params(), &block(1, calls)).is_empty());
    }

    #[test]
    fn an_event_under_a_router_call_that_is_not_a_swap_is_ignored() {
        let calls =
            vec![call(1, 0, ROUTER, "deadbeef", vec![]), call(2, 1, PAYER, "", vec![paid_log(5)])];

        assert!(subsidy_events(&params(), &block(1, calls)).is_empty());
    }

    #[test]
    fn a_failed_transaction_paid_nothing() {
        let block = block(2, calls_through_the_router(vec![paid_log(29)]));

        assert!(subsidy_events(&params(), &block).is_empty());
    }

    #[test]
    fn a_reverted_payer_call_paid_nothing() {
        let mut calls = calls_through_the_router(vec![paid_log(29)]);
        calls[3].state_reverted = true;

        assert!(subsidy_events(&params(), &block(1, calls)).is_empty());
    }

    #[test]
    fn a_cyclic_trace_ends_the_walk() {
        let calls = vec![call(1, 2, OTHER, "", vec![]), call(2, 1, PAYER, "", vec![paid_log(5)])];

        assert!(subsidy_events(&params(), &block(1, calls)).is_empty());
    }

    #[test]
    fn the_index_keeps_a_block_with_a_payer_log_from_any_emitter() {
        let mut log = paid_log(0);
        log.address = vec![0x55; 20];

        assert!(is_subsidy_log(&log));
        assert!(is_subsidy_log(&skipped_log(1)));
        assert!(!is_subsidy_log(&Log { topics: vec![vec![0xcc; 32]], ..Default::default() }));
    }

    #[test]
    fn a_nonce_is_kept_as_the_word_it_was_signed_as() {
        assert_eq!(word(&BigInt::from(1)), word_of("1"));
        assert_eq!(word(&BigInt::zero()), vec![0u8; 32]);
    }
}
