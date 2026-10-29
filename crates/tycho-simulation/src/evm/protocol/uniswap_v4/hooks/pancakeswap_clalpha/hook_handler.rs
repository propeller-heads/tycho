use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, aliases::U24, Address, I256, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext},
    },
    Bytes,
};

use crate::evm::protocol::uniswap_v4::{
    hooks::{
        hook_handler::HookHandler,
        models::{
            AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapDelta, BeforeSwapOutput,
            BeforeSwapParameters, SwapParams, WithGasEstimate,
        },
        utils::HookOptions,
    },
    state::UniswapV4State,
};

/// The four `CLAlphaHook` deployments on BSC, all verified on BscScan. `0x9a9b…` and `0x72e0…`
/// share one runtime bytecode; the other two are later versions. Every version has the same
/// `beforeSwap`.
pub const CLALPHA_HOOKS_BSC: [Address; 4] = [
    address!("9a9b5331ce8d74b2b721291d57de696e878353fd"),
    address!("72e09ebd9b24f47730b651889a4ed984cba53d90"),
    address!("b0baa371b899950b4ef6a27c21baf5ef7c434d0f"),
    address!("b0bb171d333569cfd28a37f5c5dddaaa90ad46af"),
];

/// The first `CLAlphaHook` version, deployed twice. It is built without the optimizer, so its
/// `beforeSwap` costs more than the later versions'.
pub const CLALPHA_V1_HOOKS_BSC: [Address; 2] = [CLALPHA_HOOKS_BSC[0], CLALPHA_HOOKS_BSC[1]];

/// Gas a first-version `CLAlphaHook` adds to a swap, end to end: the pool manager encoding the
/// call, the cold call into the hook, its `poolStartedTimestamp[id]` lookup and decoding the
/// result. Measured on a BSC fork at block 127744000 as `CLPoolManager.swap` with the call minus
/// the same swap with the call skipped: 11068 on both deployments.
pub const CLALPHA_V1_BEFORE_SWAP_GAS: u64 = 11_100;

/// The same end-to-end gas for the later `CLAlphaHook` versions, measured the same way: 7536 on
/// `0xb0ba…` and 7580 on `0xb0bb…`.
pub const CLALPHA_BEFORE_SWAP_GAS: u64 = 7_600;

/// A PancakeSwap Infinity `CLAlphaHook`: a start-time gate in front of an otherwise ordinary
/// concentrated-liquidity pool.
///
/// The deployed `_beforeSwap` reverts `PoolNotStarted` unless
/// `poolStartedTimestamp[id] > 0 && poolStartedTimestamp[id] <= block.timestamp`, and otherwise
/// returns a zero delta and no fee override on a static-fee pool. Once the pool is open the hook
/// changes nothing about a swap, so the pool's own math prices it.
///
/// The hook owner can move the start time until the pool opens, and on the first version even
/// after it has opened, which pauses the pool. The indexer publishes every change as the
/// `alpha_start_timestamp` attribute, and the gate is evaluated, conservatively, against the
/// block a quote is expected to execute in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PancakeswapClAlphaHookHandler {
    address: Address,
    /// `poolStartedTimestamp[id]`. Zero means the pool is disabled.
    start_timestamp: u64,
    /// Timestamp of the block a quote is expected to execute in.
    execution_timestamp: u64,
}

impl PancakeswapClAlphaHookHandler {
    pub fn new(address: Address, start_timestamp: u64, execution_timestamp: u64) -> Self {
        Self { address, start_timestamp, execution_timestamp }
    }

    /// Whether a swap is sure to pass the gate, which opens once
    /// `0 < poolStartedTimestamp[id] <= block.timestamp`.
    ///
    /// The comparison is strict because the execution timestamp is projected: on BSC it is the
    /// last block's timestamp plus one second, but about two blocks share each second, so the
    /// block a quote lands in can still carry the last block's timestamp. A strict comparison
    /// opens the pool only once that block has reached the start time.
    pub fn is_open(&self) -> bool {
        self.start_timestamp > 0 && self.start_timestamp < self.execution_timestamp
    }

    fn before_swap_gas(&self) -> u64 {
        if CLALPHA_V1_HOOKS_BSC.contains(&self.address) {
            CLALPHA_V1_BEFORE_SWAP_GAS
        } else {
            CLALPHA_BEFORE_SWAP_GAS
        }
    }
}

impl HookHandler for PancakeswapClAlphaHookHandler {
    fn address(&self) -> Address {
        self.address
    }

    /// The hook registers `beforeSwap` and no other swap callback. Infinity reads permissions
    /// from the pool key's `parameters`, so the address bits the default checks mean nothing here.
    fn has_permission(&self, option: HookOptions) -> bool {
        option == HookOptions::BeforeSwap
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let was_open = self.is_open();
        self.execution_timestamp = block.timestamp();
        was_open != self.is_open()
    }

    fn swaps_enabled(&self) -> bool {
        self.is_open()
    }

    /// The `beforeSwap` gas estimate is measured end to end and already includes the dispatch.
    fn pool_manager_call_overhead(&self) -> u64 {
        0
    }

    /// Rejects the swap while the pool is closed, as `_beforeSwap` does with `PoolNotStarted`;
    /// otherwise lets it through untouched.
    fn before_swap(
        &self,
        _params: BeforeSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        if !self.is_open() {
            return Err(SimulationError::InvalidInput(
                format!(
                    "clalpha hook {}: PoolNotStarted (start {}, execution block at {})",
                    self.address, self.start_timestamp, self.execution_timestamp
                ),
                None,
            ));
        }
        Ok(WithGasEstimate {
            gas_estimate: self.before_swap_gas(),
            result: BeforeSwapOutput {
                amount_delta: BeforeSwapDelta(I256::ZERO),
                fee: U24::ZERO,
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
    }

    /// Never called: the hook has no `afterSwap` permission.
    fn after_swap(
        &self,
        _params: AfterSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        Err(SimulationError::RecoverableError("clalpha hook has no afterSwap".into()))
    }

    /// The hook takes no fee.
    fn fee(&self, _context: &UniswapV4State, _params: SwapParams) -> Result<f64, SimulationError> {
        Ok(0.0)
    }

    /// Never consulted: [`Self::unspecified_fee_amount`] always answers, so the pool prices
    /// itself.
    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "spot_price is not implemented for PancakeswapClAlphaHookHandler".into(),
        ))
    }

    /// The hook keeps nothing of a swap. Answers even while the pool is closed, so a pool keeps a
    /// spot price through a pause; `before_swap` and `get_limits` are what stop it from trading.
    fn unspecified_fee_amount(
        &self,
        _unspecified: U256,
        _zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        Ok(Some(U256::ZERO))
    }

    /// Reports "not implemented", which `get_limits` reads as a signal to derive the limits from
    /// the pool's own liquidity.
    fn get_amount_ranges(
        &self,
        _token_in: Bytes,
        _token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError> {
        Err(SimulationError::RecoverableError(
            "get_amount_ranges is not implemented for PancakeswapClAlphaHookHandler".into(),
        ))
    }

    /// Picks up a new start time. A deleted attribute leaves the pool closed, as an unset
    /// `poolStartedTimestamp` would.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(raw) = delta
            .updated_attributes
            .get(super::START_TIMESTAMP_ATTRIBUTE)
        {
            self.start_timestamp = super::decode_start_timestamp(raw)
                .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
        } else if delta
            .deleted_attributes
            .contains(super::START_TIMESTAMP_ATTRIBUTE)
        {
            self.start_timestamp = 0;
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn HookHandler> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_equal(&self, other: &dyn HookHandler) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use alloy::primitives::Address;
    use rstest::rstest;

    use super::*;
    use crate::evm::protocol::uniswap_v4::{
        hooks::{
            models::StateContext,
            pancakeswap_clalpha::{decode_start_timestamp, START_TIMESTAMP_ATTRIBUTE},
        },
        state::UniswapV4Fees,
    };

    const HOOK: Address = CLALPHA_HOOKS_BSC[2];

    fn before_swap_params() -> BeforeSwapParameters {
        BeforeSwapParameters {
            context: StateContext {
                currency_0: Address::ZERO,
                currency_1: Address::repeat_byte(1),
                fees: UniswapV4Fees::new(0, 0, 67),
                tick_spacing: 10,
            },
            sender: Address::ZERO,
            swap_params: SwapParams {
                zero_for_one: true,
                amount_specified: I256::try_from(-1_000).unwrap(),
                sqrt_price_limit: U256::ZERO,
            },
            hook_data: Bytes::new(),
        }
    }

    fn delta(updated: &[(&str, Bytes)], deleted: &[&str]) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: "pool".into(),
            updated_attributes: updated
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
            deleted_attributes: deleted
                .iter()
                .map(|name| name.to_string())
                .collect::<HashSet<_>>(),
        }
    }

    /// A zero start never opens, and the projected execution second must be past the start.
    #[rstest]
    #[case::disabled(0, 5_000, false)]
    #[case::before_start(2_000, 1_999, false)]
    #[case::at_start(2_000, 2_000, false)]
    #[case::one_second_after_start(2_000, 2_001, true)]
    #[case::after_start(2_000, 9_999, true)]
    fn gate_matches_is_pool_started(#[case] start: u64, #[case] now: u64, #[case] open: bool) {
        let handler = PancakeswapClAlphaHookHandler::new(HOOK, start, now);
        assert_eq!(handler.is_open(), open);
        assert_eq!(handler.swaps_enabled(), open);
    }

    #[test]
    fn apply_block_reports_only_a_gate_flip() {
        let mut handler = PancakeswapClAlphaHookHandler::new(HOOK, 2_000, 1_000);

        assert!(!handler.apply_block(&BlockContext::new(10, 2_000)), "still closed");
        assert!(handler.apply_block(&BlockContext::new(11, 2_001)), "opens past the start second");
        assert!(!handler.apply_block(&BlockContext::new(11, 2_001)), "a repeated block is a no-op");
        assert!(!handler.apply_block(&BlockContext::new(12, 2_003)), "stays open");
    }

    #[test]
    fn before_swap_rejects_a_closed_pool_with_invalid_input() {
        let handler = PancakeswapClAlphaHookHandler::new(HOOK, 2_000, 1_000);

        let err = handler
            .before_swap(before_swap_params(), None, None)
            .unwrap_err();

        let SimulationError::InvalidInput(message, None) = err else {
            panic!("a closed pool is an input the pool refuses, not a failure: {err:?}")
        };
        assert!(message.contains("PoolNotStarted"), "{message}");
    }

    #[rstest]
    #[case::v1(CLALPHA_HOOKS_BSC[0], CLALPHA_V1_BEFORE_SWAP_GAS)]
    #[case::v1_second_deployment(CLALPHA_HOOKS_BSC[1], CLALPHA_V1_BEFORE_SWAP_GAS)]
    #[case::v2(CLALPHA_HOOKS_BSC[2], CLALPHA_BEFORE_SWAP_GAS)]
    #[case::v3(CLALPHA_HOOKS_BSC[3], CLALPHA_BEFORE_SWAP_GAS)]
    fn before_swap_passes_an_open_pool_through_untouched(#[case] hook: Address, #[case] gas: u64) {
        let handler = PancakeswapClAlphaHookHandler::new(hook, 2_000, 2_001);

        let out = handler
            .before_swap(before_swap_params(), None, None)
            .unwrap();

        assert_eq!(out.gas_estimate, gas);
        assert_eq!(handler.pool_manager_call_overhead(), 0, "the estimate covers the dispatch");
        assert_eq!(out.result.amount_delta.as_i256(), I256::ZERO);
        assert_eq!(out.result.fee, U24::ZERO, "no LP fee override");
        assert!(out.result.overwrites.is_empty());
    }

    /// Infinity takes permissions from the pool key, so the address bits must not leak in:
    /// `0xb0ba…4d0f` has the v4 `beforeSwap` bit clear and both returns-delta bits set.
    #[rstest]
    #[case::before_swap(HookOptions::BeforeSwap, true)]
    #[case::after_swap(HookOptions::AfterSwap, false)]
    #[case::before_swap_returns_delta(HookOptions::BeforeSwapReturnsDelta, false)]
    #[case::after_swap_returns_delta(HookOptions::AfterSwapReturnsDelta, false)]
    fn permissions_ignore_the_address_bits(#[case] option: HookOptions, #[case] expected: bool) {
        let handler = PancakeswapClAlphaHookHandler::new(HOOK, 1, 1);
        assert_eq!(handler.has_permission(option), expected);
    }

    #[test]
    fn delta_transition_moves_the_start_time() {
        let mut handler = PancakeswapClAlphaHookHandler::new(HOOK, 2_000, 3_000);
        assert!(handler.is_open());

        // The owner pushes the start past the execution block: the pool pauses.
        handler
            .delta_transition(
                delta(
                    &[(START_TIMESTAMP_ATTRIBUTE, Bytes::from(5_000u64.to_be_bytes().to_vec()))],
                    &[],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert!(!handler.is_open());

        // A deleted attribute reads as an unset `poolStartedTimestamp`.
        handler
            .delta_transition(
                delta(&[], &[START_TIMESTAMP_ATTRIBUTE]),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert_eq!(handler, PancakeswapClAlphaHookHandler::new(HOOK, 0, 3_000));
    }

    #[test]
    fn delta_transition_ignores_other_attributes() {
        let mut handler = PancakeswapClAlphaHookHandler::new(HOOK, 2_000, 3_000);
        handler
            .delta_transition(
                delta(&[("liquidity", Bytes::from(vec![1]))], &["ticks/10/net-liquidity"]),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert_eq!(handler, PancakeswapClAlphaHookHandler::new(HOOK, 2_000, 3_000));
    }

    /// `to_signed_bytes_be` is minimal-width and prepends a zero byte when the top bit is set.
    #[rstest]
    #[case::zero(vec![0x00], 0)]
    #[case::empty(vec![], 0)]
    #[case::one_byte(vec![0x64], 100)]
    #[case::sign_padded(vec![0x00, 0x80], 128)]
    #[case::timestamp(1_759_834_800u64.to_be_bytes().to_vec(), 1_759_834_800)]
    #[case::past_u64_saturates(vec![0x01; 20], u64::MAX)]
    #[case::uint256_max_padded([vec![0x00], vec![0xff; 32]].concat(), u64::MAX)]
    fn decodes_the_start_timestamp(#[case] raw: Vec<u8>, #[case] expected: u64) {
        assert_eq!(decode_start_timestamp(&Bytes::from(raw)).unwrap(), expected);
    }

    #[test]
    fn rejects_a_start_timestamp_wider_than_uint256() {
        assert!(decode_start_timestamp(&Bytes::from(vec![0x01; 33])).is_err());
    }
}
