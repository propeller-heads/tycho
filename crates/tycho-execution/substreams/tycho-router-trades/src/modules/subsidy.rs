//! What the `SubsidyPayer` did with each signed subsidy: paid it (`SubsidyPaid`), or skipped it
//! for a reason it names (`SubsidySkipped`), the swap filling either way.
//!
//! Each event is joined to its trade through the call trace: the payer runs inside the router
//! call that `map_trades` stores, so the nearest enclosing router call with a swap selector is
//! the trade the subsidy belongs to. A transaction can hold several router calls, probes
//! included, so the transaction hash alone would not say which.
use std::collections::HashMap;

use anyhow::Result;
use substreams::scalar::BigInt;
use substreams_ethereum::{
    pb::eth::v2::{Block, Call, Log, TransactionTrace},
    Event,
};

use super::block_timestamp;
use crate::{
    abi::subsidy_payer::events::{SubsidyPaid, SubsidySkipped},
    decode::decode_swap_call,
    params::Params,
    pb::tycho::router::v1::{SubsidyEvent, SubsidyEvents},
};

/// Emits every `SubsidyPaid` and `SubsidySkipped` the configured payer logged in a call the
/// chain kept. Without a `payer` in the params it emits nothing.
#[substreams::handlers::map]
pub fn map_subsidy_events(params: String, block: Block) -> Result<SubsidyEvents> {
    let params = Params::parse(&params)?;
    Ok(SubsidyEvents { events: subsidy_events(&params, &block) })
}

/// Whether the log is a `SubsidyPaid` or `SubsidySkipped`, from any emitter. The block index
/// uses it to keep the blocks `map_subsidy_events` needs; it knows no payer.
pub(crate) fn is_subsidy_log(log: &Log) -> bool {
    SubsidyPaid::match_log(log) || SubsidySkipped::match_log(log)
}

/// The subsidy events of one block, read from the calls of successful transactions, and only
/// from calls whose state the chain kept: a payment in a reverted call moved nothing.
fn subsidy_events(params: &Params, block: &Block) -> Vec<SubsidyEvent> {
    let Some(payer) = params.payer.as_deref() else {
        return Vec::new();
    };
    let timestamp = block_timestamp(block);
    let mut events = Vec::new();
    for tx in block.transactions() {
        let calls: HashMap<u32, &Call> = tx
            .calls
            .iter()
            .map(|call| (call.index, call))
            .collect();
        for call in &tx.calls {
            if call.address != payer || call.state_reverted {
                continue;
            }
            for log in &call.logs {
                let Some(decoded) = decode_event(log) else {
                    continue;
                };
                events.push(SubsidyEvent {
                    chain: params.chain.clone(),
                    block_number: block.number,
                    block_timestamp: timestamp,
                    tx_hash: tx.hash.clone(),
                    log_index: log.index,
                    payer: payer.to_vec(),
                    event: decoded.event.to_string(),
                    nonce: decoded.nonce,
                    bound_to: decoded.bound_to,
                    receiver: decoded.receiver,
                    token: decoded.token,
                    amount: decoded.amount,
                    reason: decoded.reason,
                    trade_call_index: enclosing_trade(params, &calls, call, tx),
                });
            }
        }
    }
    events
}

/// One decoded payer event; the fields of the other kind stay empty.
struct Decoded {
    event: &'static str,
    nonce: Vec<u8>,
    bound_to: Vec<u8>,
    receiver: Vec<u8>,
    token: Vec<u8>,
    amount: String,
    reason: String,
}

fn decode_event(log: &Log) -> Option<Decoded> {
    if let Some(paid) = SubsidyPaid::match_and_decode(log) {
        return Some(Decoded {
            event: "paid",
            nonce: word(&paid.nonce),
            bound_to: paid.bound_to,
            receiver: paid.to,
            token: paid.token,
            amount: paid.amount.to_string(),
            reason: String::new(),
        });
    }
    let skipped = SubsidySkipped::match_and_decode(log)?;
    Some(Decoded {
        event: "skipped",
        nonce: word(&skipped.nonce),
        bound_to: Vec::new(),
        receiver: Vec::new(),
        token: Vec::new(),
        amount: String::new(),
        reason: skip_reason(&skipped.reason),
    })
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
/// selector is what tells the trade from it.
fn enclosing_trade(
    params: &Params,
    calls: &HashMap<u32, &Call>,
    call: &Call,
    tx: &TransactionTrace,
) -> Option<u32> {
    let mut current = call;
    let mut hops = 0;
    while current.parent_index != 0 && hops < tx.calls.len() {
        current = calls.get(&current.parent_index)?;
        hops += 1;
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
    use super::*;

    const PAYER: &str = "bfdf58dc941cb83a32afe0fc008288d7ea509e7c";
    const ROUTER: &str = "aba5b53b03eafad1c5fc8bd5fc765fc85bb3de67";
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

    fn params(payer: bool) -> Params {
        let payer = if payer { format!("&payer=0x{PAYER}") } else { String::new() };
        Params::parse(&format!("chain=base&routers=0x{ROUTER}:v3_1{payer}")).unwrap()
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
            call(1, 0, "1111111111111111111111111111111111111111", "", vec![]),
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

    #[test]
    fn a_payment_is_decoded_and_joined_to_its_trade() {
        let events =
            subsidy_events(&params(true), &block(1, calls_through_the_router(vec![paid_log(29)])));

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event, "paid");
        assert_eq!(hex::encode(&event.nonce), NONCE);
        assert_eq!(hex::encode(&event.bound_to), "b8df65dd1ff912bc3159c3d74ea134cc8da56aa5");
        assert_eq!(
            hex::encode(&event.receiver),
            ROUTER,
            "the payer pays the router, which pays the user"
        );
        assert_eq!(hex::encode(&event.token), USDC);
        assert_eq!(event.amount, "874");
        assert_eq!(event.reason, "");
        assert_eq!(event.log_index, 29);
        assert_eq!(event.block_number, 53844164);
        assert_eq!(hex::encode(&event.payer), PAYER);
        assert_eq!(event.trade_call_index, Some(2), "the swap call, not the executor's frame");
    }

    #[test]
    fn a_skip_carries_its_reason() {
        let reasons: Vec<(String, String)> = subsidy_events(
            &params(true),
            &block(1, calls_through_the_router(vec![skipped_log(7), skipped_log(9)])),
        )
        .into_iter()
        .map(|event| (event.event, event.reason))
        .collect();

        assert_eq!(
            reasons,
            [
                ("skipped".to_string(), "insufficient_balance".to_string()),
                ("skipped".to_string(), "9".to_string())
            ]
        );
    }

    #[test]
    fn a_skip_has_no_payment_fields() {
        let events = subsidy_events(
            &params(true),
            &block(1, calls_through_the_router(vec![skipped_log(1)])),
        );

        assert_eq!(events.len(), 1);
        assert!(
            events[0].bound_to.is_empty() &&
                events[0].token.is_empty() &&
                events[0].amount.is_empty()
        );
        assert_eq!(hex::encode(&events[0].nonce), NONCE);
    }

    #[test]
    fn a_payer_call_outside_a_router_call_has_no_trade() {
        let calls = vec![
            call(1, 0, "1111111111111111111111111111111111111111", "", vec![]),
            call(2, 1, PAYER, "", vec![paid_log(5)]),
        ];

        let events = subsidy_events(&params(true), &block(1, calls));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].trade_call_index, None);
    }

    #[test]
    fn without_a_payer_nothing_is_emitted() {
        let block = block(1, calls_through_the_router(vec![paid_log(29)]));
        assert!(subsidy_events(&params(false), &block).is_empty());
    }

    #[test]
    fn another_emitter_is_ignored() {
        let mut log = paid_log(29);
        log.address = vec![0x55; 20];
        let mut calls = calls_through_the_router(vec![]);
        calls.push(call(5, 2, &"55".repeat(20), "", vec![log]));

        assert!(subsidy_events(&params(true), &block(1, calls)).is_empty());
    }

    #[test]
    fn a_failed_transaction_paid_nothing() {
        let block = block(2, calls_through_the_router(vec![paid_log(29)]));
        assert!(subsidy_events(&params(true), &block).is_empty());
    }

    #[test]
    fn a_reverted_payer_call_paid_nothing() {
        let mut calls = calls_through_the_router(vec![paid_log(29)]);
        calls[3].state_reverted = true;

        assert!(subsidy_events(&params(true), &block(1, calls)).is_empty());
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
