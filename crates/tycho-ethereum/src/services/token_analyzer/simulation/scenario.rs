//! Simulates the buy, transfer and sell legs of one scenario with `eth_simulateV1`.
//!
//! The buy is the pool calling `token.transfer(trader, amount)`, as a pool does when it pays
//! out a swap. In the next block the trader, running `Runner` code, forwards half to a wallet
//! and sells the rest by transferring it to the pool, measuring what arrived. No router is
//! involved, so the same legs work for any pool that holds the token, and the pool's own code
//! stays intact for tokens that swap their fees through it.

use std::collections::BTreeMap;

use alloy::{
    primitives::{keccak256, Address, Bytes, B256, U256, U64},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        BlockNumberOrTag, BlockOverrides, Log, TransactionInput, TransactionRequest,
    },
    sol_types::{Panic, Revert, SolCall, SolError},
};
use serde::Deserialize;

use super::runner::{legsCall, recordCall, LegResult, RUNNER_BYTECODE};
use crate::{
    erc20::transferCall,
    rpc::{errors::RPCError, EthereumRpcClient},
};

/// The most blocks a scenario skips: time shifts move the block number by at most this while
/// moving the timestamp in full, and the legs run this long after the calls before them.
/// `eth_simulateV1` materialises every skipped block, which dominates the cost of the scenario,
/// and refuses gaps above 256. 50 blocks outlast block-counted launch windows.
pub(super) const MAX_BLOCK_SKIP: u64 = 50;
const CALL_GAS: u64 = 5_000_000;

/// One call's result from `eth_simulateV1`. Kept separate from alloy's type because providers
/// put revert data in `error.data`, which alloy's type drops.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SimCall {
    pub return_data: Bytes,
    #[serde(default)]
    pub logs: Vec<Log>,
    pub status: U64,
    #[serde(default)]
    pub error: Option<SimCallError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct SimCallError {
    pub message: String,
    #[serde(default)]
    pub data: Option<Bytes>,
}

impl SimCall {
    pub fn succeeded(&self) -> bool {
        !self.status.is_zero()
    }

    /// The revert reason, from `error.data` or `returnData`, whichever carries it.
    pub fn revert_reason(&self) -> String {
        let data = match &self.error {
            Some(SimCallError { data: Some(data), .. }) if self.return_data.is_empty() => data,
            Some(_) | None => &self.return_data,
        };
        let message = self
            .error
            .as_ref()
            .map(|e| e.message.as_str());
        revert_reason(data, message)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct SimBlockCalls {
    pub calls: Vec<SimCall>,
}

/// Human-readable revert reason from revert `data`; `message` is the node's error text, used
/// only when there is no data.
pub(super) fn revert_reason(data: &[u8], message: Option<&str>) -> String {
    if data.is_empty() {
        return match message {
            Some(m) if !m.contains("execution reverted") => m.to_string(),
            Some(_) | None => "reverted without a reason".to_string(),
        };
    }
    if let Ok(revert) = Revert::abi_decode(data) {
        return revert.reason;
    }
    if let Ok(panic) = Panic::abi_decode(data) {
        return format!("panic {:#x}", panic.code);
    }
    format!("custom error {}", Bytes::copy_from_slice(&data[..data.len().min(4)]))
}

/// Size of the buy, relative to the pool's balance of the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuySize {
    /// 0.5% of the pool's balance: below typical max-transaction limits.
    Small,
    /// 2% of the pool's balance: large enough to hit max-transaction and max-wallet limits.
    Large,
}

/// Distance between two simulated blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockGap {
    pub blocks: u64,
    pub secs: u64,
}

/// When the buy runs relative to the prefix. The sell always runs in the block after the buy, as
/// for a holder selling later: a sell in the buy's block would trip anti-sandwich rules that
/// never touch a router's users, and would miss tokens that refuse sells only from the next
/// block on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegTiming {
    /// Prefix and buy in the first simulated block.
    BuyWithPrefix,
    /// The buy in a block `BlockGap` after the prefix: anti-bot windows that start when trading
    /// opens are over by then.
    BuyAfterPrefix(BlockGap),
}

impl LegTiming {
    /// The simulated blocks, each with its distance from the one before: the prefix and the buy
    /// as `timing` says, then the sell.
    /// `next` is the distance to the next block.
    pub fn blocks(
        self,
        next: BlockGap,
        prefix: Vec<TransactionRequest>,
        buy: Vec<TransactionRequest>,
        sell: Vec<TransactionRequest>,
    ) -> Vec<(BlockGap, Vec<TransactionRequest>)> {
        match self {
            LegTiming::BuyWithPrefix => vec![(next, [prefix, buy].concat()), (next, sell)],
            LegTiming::BuyAfterPrefix(gap) => vec![(next, prefix), (gap, buy), (next, sell)],
        }
    }
}

/// The block `gap` after `block`, with a new `prevrandao` when `block` has one, and its beacon
/// root, which blocks after Cancun need.
pub(super) fn block_after(block: &BlockOverrides, gap: BlockGap) -> BlockOverrides {
    BlockOverrides {
        number: block
            .number
            .map(|n| n + U256::from(gap.blocks)),
        time: block.time.map(|t| t + gap.secs),
        random: block.random.map(keccak256),
        beacon_root: block.beacon_root,
        ..Default::default()
    }
}

/// What to change before running the legs.
#[derive(Debug, Clone)]
pub(super) struct Scenario {
    pub name: String,
    /// Calls executed before the legs, e.g. an owner calling `enableTrading()`.
    pub prefix: Vec<TransactionRequest>,
    pub size: BuySize,
    pub time_shift_days: u64,
    pub timing: LegTiming,
}

impl Scenario {
    /// A scenario with the small buy and no time shift.
    pub fn new(name: &str, prefix: Vec<TransactionRequest>, timing: LegTiming) -> Self {
        Self { name: name.to_string(), prefix, size: BuySize::Small, time_shift_days: 0, timing }
    }
}

/// The result of one scenario.
#[derive(Debug, Clone)]
pub(super) struct Outcome {
    /// Whether each prefix call succeeded.
    pub prefix_ok: Vec<bool>,
    pub sent: U256,
    /// `None` when the buy succeeded, else the reason it reverted.
    pub buy_revert: Option<String>,
    /// The measured legs; `None` when the buy reverted or `legs` itself reverted.
    pub legs: Option<LegResult>,
    /// What the sell delivered to the pool: the smaller of the pool's balance change and the
    /// `Transfer` events from the seller to the pool. A token selling its own fees through the
    /// pool during the sell raises the first; a token emitting events without moving balances
    /// raises the second. Only the balance change when the seller logged no `Transfer`.
    pub sell_arrived: U256,
    /// Logs emitted during the transfer and sell.
    pub leg_logs: Vec<Log>,
}

impl Outcome {
    /// Every leg succeeded and the sell delivered tokens to the pool.
    pub fn all_legs_ok(&self) -> bool {
        self.legs.as_ref().is_some_and(|l| {
            !l.bought.is_zero() && l.transferOk && l.sellOk && !self.sell_arrived.is_zero()
        })
    }

    /// The buy and the transfer worked, but the sell failed or delivered nothing to the pool.
    pub fn only_sell_failed(&self) -> bool {
        self.legs.as_ref().is_some_and(|l| {
            !l.bought.is_zero() && l.transferOk && (!l.sellOk || self.sell_arrived.is_zero())
        })
    }

    /// Every leg delivered exactly the amount sent: no fee, no rebasing, not even a rounding
    /// loss too small to show in basis points.
    pub fn delivered_exact_amounts(&self) -> bool {
        self.legs.as_ref().is_some_and(|l| {
            l.bought == self.sent &&
                l.transferReceived == l.transferSent &&
                self.sell_arrived == l.sellSent
        })
    }

    /// At least one owner call went through and every leg succeeded after it.
    pub fn unlocked(&self) -> bool {
        self.prefix_ok.iter().any(|ok| *ok) && self.all_legs_ok()
    }

    pub fn fees(&self) -> tycho_common::models::token::LegFees {
        let Some(legs) = &self.legs else { return Default::default() };
        let mut fees = tycho_common::models::token::LegFees {
            buy: tax_bps(self.sent, legs.bought),
            ..Default::default()
        };
        if legs.transferOk {
            fees.transfer = tax_bps(legs.transferSent, legs.transferReceived);
        }
        if legs.sellOk {
            fees.sell = tax_bps(legs.sellSent, self.sell_arrived);
        }
        fees
    }

    /// Average gas of the wallet transfer and the sell, when both ran.
    pub fn transfer_gas(&self) -> Option<u64> {
        let legs = self.legs.as_ref()?;
        let total = legs
            .transferGas
            .checked_add(legs.sellGas)?;
        u64::try_from(total / U256::from(2)).ok()
    }

    /// Why the legs did not all succeed; `None` when they did.
    pub fn failure_reason(&self) -> Option<String> {
        if let Some(reason) = &self.buy_revert {
            return Some(format!("buy reverted: {reason}"));
        }
        let Some(legs) = &self.legs else {
            return Some("leg measurement reverted".to_string());
        };
        if legs.bought.is_zero() {
            return Some("buy delivered no tokens".to_string());
        }
        if !legs.transferOk {
            return Some(format!(
                "transfer reverted: {}",
                revert_reason(&legs.transferRevert, None)
            ));
        }
        if !legs.sellOk {
            return Some(format!("sell reverted: {}", revert_reason(&legs.sellRevert, None)));
        }
        if self.sell_arrived.is_zero() {
            return Some("sell succeeded but delivered nothing to the pool".to_string());
        }
        None
    }
}

/// Share of `expected` that did not arrive, in basis points; negative if more arrived.
/// `None` when nothing was expected.
pub(super) fn tax_bps(expected: U256, actual: U256) -> Option<i64> {
    if expected.is_zero() {
        return None;
    }
    let scale = U256::from(10_000);
    let (diff, sign) =
        if actual <= expected { (expected - actual, 1) } else { (actual - expected, -1) };
    let bps = diff.saturating_mul(scale) / expected;
    Some(sign * i64::try_from(bps).unwrap_or(i64::MAX))
}

/// Fixed context shared by all scenarios of one token at one block.
#[derive(Clone)]
pub(super) struct TokenContext {
    pub rpc: EthereumRpcClient,
    pub token: Address,
    pub pool: Address,
    pub block: u64,
    pub block_time: u64,
    /// Seconds between two blocks of the chain, by which each simulated block moves the time.
    pub block_secs: u64,
    pub trader: Address,
    pub wallet: Address,
    pub pool_balance: U256,
    /// Storage slots that every simulation of the token runs with, on top of the chain's
    /// state, by contract.
    pub storage: BTreeMap<Address, BTreeMap<B256, B256>>,
    /// Code that every simulation of the token runs with, by address.
    pub code: BTreeMap<Address, Bytes>,
}

impl TokenContext {
    pub fn new(
        rpc: &EthereumRpcClient,
        token: Address,
        pool: Address,
        pool_balance: U256,
        (block, block_time, block_secs): (u64, u64, u64),
    ) -> Self {
        Self {
            rpc: rpc.clone(),
            token,
            pool,
            block,
            block_time,
            block_secs,
            trader: derived_address(b"tycho-token-analysis-trader"),
            wallet: derived_address(b"tycho-token-analysis-wallet"),
            pool_balance,
            storage: BTreeMap::new(),
            code: BTreeMap::new(),
        }
    }

    pub fn amount(&self, size: BuySize) -> U256 {
        match size {
            BuySize::Small => self.pool_balance / U256::from(200),
            BuySize::Large => self.pool_balance / U256::from(50),
        }
    }

    /// The buy: the pool pays `amount` out to the trader.
    pub fn buy_tx(&self, amount: U256) -> TransactionRequest {
        let calldata = transferCall { _to: self.trader, _value: amount }.abi_encode();
        call(self.pool, self.token, calldata)
    }

    // The buy, the forward and the sell all originate from the pool's address. A router buys
    // and forwards in one transaction, so tokens limiting transfers per `tx.origin` and block
    // see one origin; separate origins would hide that limit.

    fn record_tx(&self) -> TransactionRequest {
        call(self.pool, self.trader, recordCall { token: self.token }.abi_encode())
    }

    /// Transfer of half the bought amount to the wallet and sale of the rest to the pool.
    pub fn legs_tx(&self) -> TransactionRequest {
        let calldata =
            legsCall { token: self.token, pool: self.pool, wallet: self.wallet }.abi_encode();
        call(self.pool, self.trader, calldata)
    }

    /// State the direct legs run with: the trader runs `Runner`.
    pub fn state(&self) -> StateOverride {
        let mut state = StateOverride::default();
        state.insert(
            self.trader,
            AccountOverride {
                code: Some(Bytes::from_static(RUNNER_BYTECODE)),
                ..Default::default()
            },
        );
        self.add_overrides(&mut state);
        state
    }

    /// Sets `slot` of `address` to `value` in `storage`.
    pub fn set_storage(&mut self, address: Address, slot: B256, value: B256) {
        self.storage
            .entry(address)
            .or_default()
            .insert(slot, value);
    }

    /// Adds `storage` and `code` to `state`.
    pub fn add_overrides(&self, state: &mut StateOverride) {
        for (address, code) in &self.code {
            state.entry(*address).or_default().code = Some(code.clone());
        }
        for (address, slots) in &self.storage {
            let diff = state
                .entry(*address)
                .or_default()
                .state_diff
                .get_or_insert_with(Default::default);
            diff.extend(
                slots
                    .iter()
                    .map(|(slot, value)| (*slot, *value)),
            );
        }
    }

    /// The distance to the next block of the chain.
    pub fn next_block(&self) -> BlockGap {
        BlockGap { blocks: 1, secs: self.block_secs }
    }

    /// The block after `self.block`, shifted by `time_shift_days`.
    pub fn first_block(&self, time_shift_days: u64) -> BlockOverrides {
        let shift_secs = time_shift_days * 86_400;
        let skipped_blocks = (shift_secs / self.block_secs.max(1)).min(MAX_BLOCK_SKIP);
        BlockOverrides {
            number: Some(U256::from(self.block + 1 + skipped_blocks)),
            time: Some(self.block_time + self.block_secs + shift_secs),
            // A shift can move the timestamp past a fork activation; after Cancun the node
            // requires a parent beacon block root.
            beacon_root: (time_shift_days > 0).then_some(B256::ZERO),
            ..Default::default()
        }
    }

    /// Simulates `scenario` with the direct legs; fails only when the simulation itself could
    /// not run.
    pub async fn run(&self, scenario: &Scenario) -> Result<Outcome, String> {
        self.run_direct(scenario).await
    }

    /// Simulates `blocks` one after the other, the first at `first` on top of `state`, and
    /// returns the result of every call in order. Fails when the simulation could not run or
    /// returned a result count other than the number of calls.
    pub async fn simulate_blocks(
        &self,
        first: BlockOverrides,
        state: StateOverride,
        validation: bool,
        blocks: Vec<(BlockGap, Vec<TransactionRequest>)>,
    ) -> Result<Vec<SimCall>, String> {
        let mut payload = SimulatePayload { validation, ..Default::default() };
        let mut expected = 0;
        let mut overrides = first;
        let mut state = Some(state);
        for (index, (gap, calls)) in blocks.into_iter().enumerate() {
            if index > 0 {
                overrides = block_after(&overrides, gap);
            }
            expected += calls.len();
            payload = payload.extend(SimBlock {
                block_overrides: Some(overrides.clone()),
                state_overrides: state.take(),
                calls,
            });
        }
        let blocks: Vec<SimBlockCalls> = self
            .rpc
            .eth_simulate_v1(&payload, BlockNumberOrTag::Number(self.block))
            .await
            .map_err(|e| e.to_string())?;
        let results: Vec<SimCall> = blocks
            .into_iter()
            .flat_map(|b| b.calls)
            .collect();
        if results.len() != expected {
            return Err(format!(
                "eth_simulateV1 returned {} call results for {expected} calls",
                results.len()
            ));
        }
        Ok(results)
    }

    async fn run_direct(&self, scenario: &Scenario) -> Result<Outcome, String> {
        let sent = self.amount(scenario.size);
        let blocks = scenario.timing.blocks(
            self.next_block(),
            scenario.prefix.clone(),
            vec![self.record_tx(), self.buy_tx(sent)],
            vec![self.legs_tx()],
        );
        let first = self.first_block(scenario.time_shift_days);
        let results = self
            .simulate_blocks(first, self.state(), false, blocks)
            .await?;

        let prefix_len = scenario.prefix.len();
        let (buy_result, legs_result) = (&results[prefix_len + 1], &results[prefix_len + 2]);
        let legs = legs_result
            .succeeded()
            .then(|| legsCall::abi_decode_returns(&legs_result.return_data).ok())
            .flatten();
        let logged = self.transferred_to_pool(&legs_result.logs);
        let sell_arrived = legs
            .as_ref()
            .map(|l| logged.map_or(l.sellArrived, |logged| l.sellArrived.min(logged)))
            .unwrap_or_default();
        Ok(Outcome {
            prefix_ok: results[..prefix_len]
                .iter()
                .map(SimCall::succeeded)
                .collect(),
            sent,
            buy_revert: (!buy_result.succeeded()).then(|| buy_result.revert_reason()),
            legs: buy_result
                .succeeded()
                .then_some(legs)
                .flatten(),
            sell_arrived,
            leg_logs: legs_result.logs.clone(),
        })
    }

    /// Sum of the token's `Transfer` events from the trader to the pool in `logs`.
    fn transferred_to_pool(&self, logs: &[Log]) -> Option<U256> {
        transferred(self.token, self.trader, self.pool, logs)
    }
}

/// Sum of `token`'s `Transfer` events from `from` to `to` in `logs`; `None` when the token
/// logged no standard `Transfer` from `from` at all, as older tokens without indexed event
/// parameters do.
pub(super) fn transferred(
    token: Address,
    from: Address,
    to: Address,
    logs: &[Log],
) -> Option<U256> {
    let topic = keccak256("Transfer(address,address,uint256)");
    let mut total = None;
    for log in logs {
        let topics = log.topics();
        let from_sender = log.address() == token &&
            topics.len() == 3 &&
            topics[0] == topic &&
            Address::from_word(topics[1]) == from;
        if !from_sender {
            continue;
        }
        let sum = total.get_or_insert(U256::ZERO);
        if Address::from_word(topics[2]) == to {
            *sum = sum.saturating_add(first_word(log.data().data.as_ref()));
        }
    }
    total
}

/// A legacy transaction with zero gas price, valid on blocks before and after London.
pub(super) fn call(from: Address, to: Address, calldata: Vec<u8>) -> TransactionRequest {
    TransactionRequest::default()
        .from(from)
        .to(to)
        .input(TransactionInput::new(Bytes::from(calldata)))
        .gas_limit(CALL_GAS)
        .gas_price(0)
}

/// The return data of a call; `None` when the call reverted or halted, an error when the node
/// failed.
pub(super) fn return_data_unless_reverted(
    result: Result<tycho_common::Bytes, RPCError>,
) -> Result<Option<tycho_common::Bytes>, RPCError> {
    match result {
        Ok(data) => Ok(Some(data)),
        Err(error) if error.is_execution_failure() => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn derived_address(seed: &[u8]) -> Address {
    Address::from_slice(&keccak256(seed)[12..])
}

/// Interprets up to the first 32 bytes as a big-endian uint256; shorter data reads as zero.
pub(super) fn first_word(data: &[u8]) -> U256 {
    data.get(..32)
        .map(U256::from_be_slice)
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::ethereum(12)]
    #[case::base(2)]
    fn simulated_blocks_move_the_time_by_the_chains_block_time(#[case] block_secs: u64) {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("client");
        let (token, pool) = (Address::repeat_byte(7), Address::repeat_byte(8));
        let context = TokenContext::new(&rpc, token, pool, U256::ZERO, (100, 1_000, block_secs));

        let first = context.first_block(0);
        assert_eq!(first.number, Some(U256::from(101)));
        assert_eq!(first.time, Some(1_000 + block_secs));
        let next = block_after(&first, context.next_block());
        assert_eq!(next.time, Some(1_000 + 2 * block_secs));

        let later = context.first_block(30);
        assert_eq!(later.number, Some(U256::from(101 + MAX_BLOCK_SKIP)));
        assert_eq!(later.time, Some(1_000 + block_secs + 30 * 86_400));
    }

    /// Legs where the transfer keeps 1% and the sell keeps `500 - arrived` of 500 sent.
    pub fn legs(transfer_ok: bool, sell_ok: bool, arrived: u64) -> LegResult {
        LegResult {
            bought: U256::from(1000),
            transferOk: transfer_ok,
            transferSent: U256::from(500),
            transferReceived: U256::from(if transfer_ok { 495 } else { 0 }),
            transferGas: U256::from(40_000),
            sellOk: sell_ok,
            sellSent: U256::from(500),
            sellArrived: U256::from(arrived),
            sellGas: U256::from(60_000),
            transferRevert: Bytes::new(),
            sellRevert: Revert { reason: "no sells".to_string() }
                .abi_encode()
                .into(),
        }
    }

    pub fn outcome(legs: Option<LegResult>) -> Outcome {
        Outcome {
            prefix_ok: vec![],
            sent: U256::from(1010),
            buy_revert: None,
            sell_arrived: legs
                .as_ref()
                .map(|l| l.sellArrived)
                .unwrap_or_default(),
            legs,
            leg_logs: vec![],
        }
    }

    #[test]
    fn tax_bps_measures_shortfall_and_surplus() {
        assert_eq!(tax_bps(U256::from(1000), U256::from(970)), Some(300));
        assert_eq!(tax_bps(U256::from(1000), U256::from(1010)), Some(-100));
        assert_eq!(tax_bps(U256::from(1000), U256::ZERO), Some(10_000));
        assert_eq!(tax_bps(U256::ZERO, U256::from(5)), None);
    }

    #[test]
    fn a_sell_that_delivers_nothing_is_a_sell_failure() {
        let ok = outcome(Some(legs(true, true, 490)));
        assert!(ok.all_legs_ok());
        assert!(!ok.only_sell_failed());
        assert_eq!(ok.failure_reason(), None);

        let phantom = outcome(Some(legs(true, true, 0)));
        assert!(!phantom.all_legs_ok());
        assert!(phantom.only_sell_failed());

        let reverted = outcome(Some(legs(true, false, 0)));
        assert!(reverted.only_sell_failed());
        assert_eq!(reverted.failure_reason().as_deref(), Some("sell reverted: no sells"));
    }

    fn transfer_log(token: Address, from: Address, to: Address, value: u64) -> Log {
        let topic = keccak256("Transfer(address,address,uint256)");
        Log {
            inner: alloy::primitives::Log {
                address: token,
                data: alloy::primitives::LogData::new_unchecked(
                    vec![topic, from.into_word(), to.into_word()],
                    U256::from(value)
                        .to_be_bytes::<32>()
                        .to_vec()
                        .into(),
                ),
            },
            ..Default::default()
        }
    }

    #[test]
    fn transferred_sums_the_events_to_the_receiver() {
        let (token, from, to) =
            (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        let elsewhere = Address::repeat_byte(4);
        let logs = [
            transfer_log(token, from, to, 70),
            transfer_log(token, from, elsewhere, 5),
            transfer_log(token, from, to, 20),
            transfer_log(elsewhere, from, to, 1000),
        ];
        assert_eq!(transferred(token, from, to, &logs), Some(U256::from(90)));
    }

    #[test]
    fn transferred_tells_no_event_apart_from_nothing_received() {
        let (token, from, to) =
            (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        assert_eq!(transferred(token, from, to, &[]), None);
        let elsewhere = [transfer_log(token, from, Address::repeat_byte(4), 5)];
        assert_eq!(transferred(token, from, to, &elsewhere), Some(U256::ZERO));
    }

    #[rstest::rstest]
    #[case::eip_1474_revert(3, "execution reverted", true)]
    #[case::invalid_jump(-32003, "EVM error: InvalidJump", true)]
    #[case::node_failure(-32000, "header not found", false)]
    fn answered_tells_a_revert_apart_from_a_node_failure(
        #[case] code: i64,
        #[case] message: &str,
        #[case] reverted: bool,
    ) {
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code,
            message: message.to_string().into(),
            data: None,
        };
        let error = RPCError::RequestError(crate::rpc::errors::RequestError::Reqwest(
            crate::rpc::errors::ReqwestError {
                msg: "eth_call failed".to_string(),
                source: alloy::transports::RpcError::ErrorResp(payload),
            },
        ));
        let answer = return_data_unless_reverted(Err(error));
        if reverted {
            assert_eq!(answer.expect("a revert is an answer"), None);
        } else {
            assert!(answer.is_err());
        }
        let data = tycho_common::Bytes::from("0x01");
        assert_eq!(return_data_unless_reverted(Ok(data.clone())).ok(), Some(Some(data)));
    }

    #[test]
    fn a_failed_transfer_is_not_a_sell_only_failure() {
        let blocked = outcome(Some(legs(false, false, 0)));
        assert!(!blocked.only_sell_failed());
        assert!(!blocked.all_legs_ok());
    }

    #[test]
    fn fees_cover_each_leg_that_ran() {
        let fees = outcome(Some(legs(true, true, 490))).fees();
        assert_eq!(fees.buy, Some(99));
        assert_eq!(fees.transfer, Some(100));
        assert_eq!(fees.sell, Some(200));
        let sell_reverted = outcome(Some(legs(true, false, 0))).fees();
        assert_eq!(sell_reverted.sell, None);
        assert_eq!(outcome(None).fees(), Default::default());
    }

    #[test]
    fn unlocked_needs_an_owner_call_that_went_through() {
        let mut opened = outcome(Some(legs(true, true, 490)));
        opened.prefix_ok = vec![false, true];
        assert!(opened.unlocked());
        opened.prefix_ok = vec![false];
        assert!(!opened.unlocked());
    }

    #[test]
    fn reads_revert_data_from_the_error_field() {
        let data = Revert { reason: "Trading not open".to_string() }.abi_encode();
        let result = SimCall {
            error: Some(SimCallError {
                message: "execution reverted: Trading not open".to_string(),
                data: Some(Bytes::from(data)),
            }),
            ..Default::default()
        };
        assert_eq!(result.revert_reason(), "Trading not open");
        let out_of_gas = SimCall {
            error: Some(SimCallError { message: "out of gas".to_string(), data: None }),
            ..Default::default()
        };
        assert_eq!(out_of_gas.revert_reason(), "out of gas");
    }
}
