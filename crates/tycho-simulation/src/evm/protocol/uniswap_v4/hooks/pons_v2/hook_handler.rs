use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address, I128, I256, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Balances,
    },
    Bytes,
};

use crate::evm::protocol::uniswap_v4::{
    hooks::{
        hook_handler::HookHandler,
        models::{
            AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapOutput,
            BeforeSwapParameters, SwapParams, WithGasEstimate,
        },
    },
    state::UniswapV4State,
};

/// Pons V2 `V2MemeHook`, deployed on Robinhood chain (id 4663) only.
///
/// The low 14 bits of the address are `0x2044`, which grants `beforeInitialize` (bit 13),
/// `afterSwap` (bit 6) and `afterSwapReturnDelta` (bit 2), and nothing else.
pub const PONS_V2_HOOK_ROBINHOOD: Address = address!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");

/// Denominator the hook's `hookFeeBps` and `creatorTaxBps` are quoted against.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// Gas the Pons `afterSwap` callback adds on top of a plain Uniswap V4 swap.
///
/// Measured on Robinhood chain at head block 69,018,230 by differencing whole-transaction
/// `gasUsed` between single-swap transactions that enter through the same router
/// (`0x8876789976decbfcbbbe364623c63652db8c0904`) on two pools with the same shape: native
/// `currency0` and tick spacing 200. Each pool has exactly one `ModifyLiquidity` event, in its
/// own creation block, opening a full-range position at ticks -887200/887200, and every swap
/// below reports that same whole liquidity in its `Swap` log, so none of them crosses a tick:
/// 29,277,002,188,455,995,649,502 across the five Pons swaps and
/// 33,133,081,655,650,444,492,657 across the four hookless ones. The public Robinhood RPC
/// serves no `debug_*`/`trace_*` method, so the callback cannot be metered on its own and this
/// is a whole-transaction difference.
///
/// Pons pool `0x4f2472…29dc` (hookFeeBps 100, creatorTaxBps 150):
///
/// | transaction   | direction    | gasUsed |
/// |---------------|--------------|---------|
/// | `0xd85d22e1…` | one-for-zero | 141,842 |
/// | `0x5464d7f0…` | one-for-zero | 141,718 |
/// | `0x0efab9a9…` | zero-for-one | 156,309 |
/// | `0x0135cadf…` | zero-for-one | 156,237 |
/// | `0xd1281cee…` | zero-for-one | 139,053 |
///
/// Hookless pool `0xdc0ca34e…9227` (`hooks == address(0)`, lp fee 33,300):
///
/// | transaction   | direction    | gasUsed |
/// |---------------|--------------|---------|
/// | `0x6d42ebc6…` | one-for-zero | 104,596 |
/// | `0xd3044474…` | one-for-zero | 110,104 |
/// | `0x52f0a338…` | zero-for-one |  99,486 |
/// | `0xeac37b06…` | zero-for-one |  99,454 |
///
/// Same-direction differences run 31,614–37,246 one-for-zero, where the hook takes its cut in
/// native currency, and 39,567–56,855 zero-for-one, where it takes an ERC-20 and pays for a
/// second token transfer. The 17,184 spread inside the zero-for-one group is one
/// zero-to-non-zero `SSTORE` on a token balance. Rounding the largest observed difference up to
/// the next 5,000 gives 60,000, which covers every sample including the cold-storage ones; the
/// warm path alone would fit in 40,000.
pub const PONS_V2_AFTER_SWAP_GAS: u64 = 60_000;

/// Native model of `V2MemeHook._afterSwap`.
///
/// The hook takes `hookFeeBps + creatorTaxBps` of the swap's unspecified currency, each term
/// floored on its own, and returns the total as a positive after-swap delta. Both rates are
/// written once by `registerPool` and are immutable for the life of the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PonsV2HookHandler {
    address: Address,
    hook_fee_bps: u16,
    creator_tax_bps: u16,
}

impl PonsV2HookHandler {
    pub fn new(address: Address, hook_fee_bps: u16, creator_tax_bps: u16) -> Self {
        Self { address, hook_fee_bps, creator_tax_bps }
    }

    /// The pool's protocol fee in basis points, paid to the protocol, buyback and creator.
    pub fn hook_fee_bps(&self) -> u16 {
        self.hook_fee_bps
    }

    /// The pool's creator tax in basis points, paid to the creator in full.
    pub fn creator_tax_bps(&self) -> u16 {
        self.creator_tax_bps
    }

    /// Returns what the hook takes out of an unspecified leg of `unspecified`:
    /// `floor(unspecified · hookFeeBps / 10_000) + floor(unspecified · creatorTaxBps / 10_000)`.
    ///
    /// The two terms are floored independently, exactly as `_afterSwap` computes them. Folding
    /// the rates into a single multiplication rounds up whenever both terms have a fractional
    /// part, so it is never equivalent. Returns an error if the intermediate product exceeds
    /// `U256`, which no on-chain delta can reach but an out-of-range rate can.
    pub fn fee_and_tax(&self, unspecified: U256) -> Result<U256, SimulationError> {
        let fee = Self::floored_bps(unspecified, self.hook_fee_bps)?;
        let tax = Self::floored_bps(unspecified, self.creator_tax_bps)?;
        fee.checked_add(tax).ok_or_else(|| {
            SimulationError::FatalError(format!(
                "pons v2 hook {}: fee {fee} plus tax {tax} overflows U256",
                self.address
            ))
        })
    }

    fn floored_bps(unspecified: U256, bps: u16) -> Result<U256, SimulationError> {
        unspecified
            .checked_mul(U256::from(bps))
            .map(|scaled| scaled / U256::from(BPS_DENOMINATOR))
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "pons v2 hook: {unspecified} times {bps} bps overflows U256"
                ))
            })
    }
}

impl HookHandler for PonsV2HookHandler {
    fn address(&self) -> Address {
        self.address
    }

    /// Always fails: bit 7 of the hook address is clear, so v4-core never calls `beforeSwap`.
    fn before_swap(
        &self,
        _params: BeforeSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        Err(SimulationError::RecoverableError("pons v2 hook has no beforeSwap".into()))
    }

    /// Charges the swap's unspecified leg and returns the take as the after-swap delta.
    ///
    /// `_afterSwap` derives the charged leg from
    /// `specifiedIsCurrency0 = (amountSpecified < 0) == zeroForOne` and charges the magnitude of
    /// the other leg, so an exact-output swap is charged on the opposite side to an exact-input
    /// swap in the same direction.
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let specified_is_currency0 =
            (params.swap_params.amount_specified < I256::ZERO) == params.swap_params.zero_for_one;
        let unspecified =
            if specified_is_currency0 { params.delta.amount1() } else { params.delta.amount0() };

        // `unsigned_abs` of an `I128` is a 128-bit unsigned value, so the width-exact conversion
        // to `u128` cannot fail, including at `I128::MIN`.
        let total = self.fee_and_tax(U256::from(unspecified.unsigned_abs().to::<u128>()))?;

        let result = u128::try_from(total)
            .ok()
            .and_then(|total| I128::try_from(total).ok())
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "pons v2 hook {}: take of {total} does not fit int128",
                    self.address
                ))
            })?;

        Ok(WithGasEstimate { gas_estimate: PONS_V2_AFTER_SWAP_GAS, result })
    }

    /// The share of the unspecified leg the hook keeps, as a fraction of one.
    fn fee(&self, _context: &UniswapV4State, _params: SwapParams) -> Result<f64, SimulationError> {
        let combined = u32::from(self.hook_fee_bps) + u32::from(self.creator_tax_bps);
        Ok(f64::from(combined) / BPS_DENOMINATOR as f64)
    }

    /// Always fails so that `UniswapV4State` derives the spot price from the pool itself. The
    /// hook never moves the price: `_afterSwap` runs after the core swap math and only accrues
    /// balances.
    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "spot_price is not implemented for PonsV2HookHandler".into(),
        ))
    }

    /// The hook's take is `fee_and_tax` of the unspecified leg, whatever the direction: the
    /// rates are fixed per pool and `_afterSwap` charges the unspecified leg both ways. Always
    /// `Some`, so the pool never has to simulate a swap to learn what a Pons quote costs.
    fn unspecified_fee_amount(
        &self,
        unspecified: U256,
        _zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        Ok(Some(self.fee_and_tax(unspecified)?))
    }

    /// Always fails with a `"not implemented"` message, which `get_limits` reads as a signal to
    /// derive the limits from the pool's own liquidity.
    fn get_amount_ranges(
        &self,
        _token_in: Bytes,
        _token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError> {
        Err(SimulationError::RecoverableError(
            "get_amount_ranges is not implemented for PonsV2HookHandler".into(),
        ))
    }

    /// Does nothing, because nothing a Pons pool publishes can change a quote.
    ///
    /// `registerPool` writes `hookFeeBps` and `creatorTaxBps` once and rejects re-registration,
    /// so the two rates this handler holds are fixed for the life of the pool. Everything
    /// `_afterSwap` writes is accounting that later sweeps read and quoting never does:
    /// `pendingFees`, `pendingCreatorTax` and `pendingBuyback`. Those three, along with
    /// `protocolFeeShareBps`, `buybackBurnBps`, `buybackEnabled` and the fee recipients, are
    /// deliberately not modelled here.
    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
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
    use std::{fs, path::Path};

    use alloy::primitives::{address, Address, I128, I256, U256};
    use rstest::rstest;
    use serde::Deserialize;
    use tycho_common::{dto::ProtocolStateDelta, simulation::errors::SimulationError, Bytes};

    use super::*;
    use crate::evm::protocol::uniswap_v4::{
        hooks::{
            hook_handler::HookHandler,
            models::{AfterSwapParameters, BalanceDelta, StateContext, SwapParams},
        },
        state::UniswapV4Fees,
    };

    const SENDER: Address = address!("8876789976decbfcbbbe364623c63652db8c0904");

    fn handler(hook_fee_bps: u16, creator_tax_bps: u16) -> PonsV2HookHandler {
        PonsV2HookHandler::new(PONS_V2_HOOK_ROBINHOOD, hook_fee_bps, creator_tax_bps)
    }

    fn after_swap_params(
        zero_for_one: bool,
        amount_specified: i128,
        amount0: i128,
        amount1: i128,
    ) -> AfterSwapParameters {
        AfterSwapParameters {
            context: StateContext {
                currency_0: Address::ZERO,
                currency_1: address!("e47a6c4f082f6a2b49d20eaca35f8d4583651f4d"),
                fees: UniswapV4Fees::new(0, 0, 0),
                tick_spacing: 200,
            },
            sender: SENDER,
            swap_params: SwapParams {
                zero_for_one,
                amount_specified: I256::try_from(amount_specified)
                    .expect("i128 always fits an I256"),
                sqrt_price_limit: U256::ZERO,
            },
            delta: BalanceDelta::new(
                I128::try_from(amount0).expect("i128 fits I128"),
                I128::try_from(amount1).expect("i128 fits I128"),
            ),
            hook_data: Bytes::new(),
        }
    }

    /// `_afterSwap` floors the fee and the tax independently. Folding the two bps into one
    /// multiplication rounds differently whenever both terms have a fractional part.
    #[rstest]
    #[case::hundred_and_two_hundred_bps(1_000_003, 100, 200, 30_000)]
    #[case::folding_would_round_up(15, 500, 500, 0)]
    #[case::folding_would_round_up_again(25, 300, 300, 0)]
    #[case::one_wei(1, 100, 100, 0)]
    #[case::below_one_bps_unit(99, 100, 0, 0)]
    #[case::exactly_one_bps_unit(100, 100, 0, 1)]
    #[case::two_independent_floors(199, 100, 100, 2)]
    #[case::zero_unspecified(0, 1_000, 1_000, 0)]
    #[case::zero_bps(1_000_003, 0, 0, 0)]
    #[case::max_hook_fee_and_tax(1_000_000, 1_000, 1_000, 200_000)]
    fn fee_and_tax_floors_each_term_independently(
        #[case] unspecified: u128,
        #[case] hook_fee_bps: u16,
        #[case] creator_tax_bps: u16,
        #[case] expected: u128,
    ) {
        let total = handler(hook_fee_bps, creator_tax_bps)
            .fee_and_tax(U256::from(unspecified))
            .expect("bounded inputs never overflow");

        assert_eq!(total, U256::from(expected));
    }

    #[test]
    fn fee_and_tax_rejects_an_unspecified_amount_that_overflows() {
        let error = handler(100, 150)
            .fee_and_tax(U256::MAX)
            .expect_err("multiplying U256::MAX by 100 overflows");

        let SimulationError::FatalError(message) = error else {
            panic!("expected a fatal arithmetic error");
        };
        assert!(message.contains("overflow"), "{message}");
    }

    /// `specifiedIsCurrency0 = (amountSpecified < 0) == zeroForOne`, and the hook charges the
    /// other leg. Tycho only quotes exact input, but the contract serves both, so all four
    /// combinations are pinned here.
    #[rstest]
    #[case::exact_input_zero_for_one(true, -1_000, -1_000, 500_000, 5_000)]
    #[case::exact_input_one_for_zero(false, -1_000, 500_000, -1_000, 5_000)]
    #[case::exact_output_zero_for_one(true, 1_000, -500_000, 1_000, 5_000)]
    #[case::exact_output_one_for_zero(false, 1_000, 1_000, -500_000, 5_000)]
    fn after_swap_charges_the_unspecified_leg(
        #[case] zero_for_one: bool,
        #[case] amount_specified: i128,
        #[case] amount0: i128,
        #[case] amount1: i128,
        #[case] expected: i128,
    ) {
        let params = after_swap_params(zero_for_one, amount_specified, amount0, amount1);

        let delta = handler(50, 50)
            .after_swap(params, None, None)
            .expect("a well formed delta is always chargeable");

        assert_eq!(delta.result, I128::try_from(expected).expect("fits I128"));
        assert_eq!(delta.gas_estimate, PONS_V2_AFTER_SWAP_GAS);
    }

    /// The contract negates a negative unspecified leg before charging, so the sign of the leg
    /// never changes what is taken.
    #[test]
    fn after_swap_charges_the_magnitude_of_the_unspecified_leg() {
        let positive = handler(100, 150)
            .after_swap(after_swap_params(true, -1_000, -1_000, 1_000_000), None, None)
            .expect("chargeable");
        let negative = handler(100, 150)
            .after_swap(after_swap_params(true, -1_000, -1_000, -1_000_000), None, None)
            .expect("chargeable");

        assert_eq!(positive.result, I128::try_from(25_000i128).expect("fits I128"));
        assert_eq!(negative.result, positive.result);
    }

    /// Whole-transaction `gasUsed` of the recorded one-for-zero swaps, where the hook takes its
    /// cut in native currency, in the order the doc comment on [`PONS_V2_AFTER_SWAP_GAS`] lists
    /// them: Pons first, then hookless.
    const ONE_FOR_ZERO_GAS_USED: (&[u64], &[u64]) = (&[141_842, 141_718], &[104_596, 110_104]);

    /// The same for the zero-for-one swaps, where the hook takes an ERC-20 instead.
    const ZERO_FOR_ONE_GAS_USED: (&[u64], &[u64]) =
        (&[156_309, 156_237, 139_053], &[99_486, 99_454]);

    /// The widest Pons-minus-hookless difference within one direction, which is how the doc
    /// comment differences them: comparing a native-currency take against an ERC-20 one would
    /// mix two costs the hook does not pay together.
    fn worst_difference_in_direction((pons, hookless): (&[u64], &[u64])) -> u64 {
        let most_expensive_pons = pons
            .iter()
            .max()
            .expect("the array is not empty");
        let cheapest_hookless = hookless
            .iter()
            .min()
            .expect("the array is not empty");
        most_expensive_pons - cheapest_hookless
    }

    /// Re-derives the constant from the measurements it came from, so the two cannot drift
    /// apart: a new number needs new evidence in the doc comment and in these arrays.
    #[test]
    fn the_after_swap_gas_constant_covers_every_measured_swap() {
        let one_for_zero = worst_difference_in_direction(ONE_FOR_ZERO_GAS_USED);
        let zero_for_one = worst_difference_in_direction(ZERO_FOR_ONE_GAS_USED);

        assert_eq!(one_for_zero, 37_246);
        assert_eq!(zero_for_one, 56_855);
        assert_eq!(
            PONS_V2_AFTER_SWAP_GAS,
            one_for_zero
                .max(zero_for_one)
                .next_multiple_of(5_000)
        );
    }

    /// Within the contract's own range the take is at most 20% of an `int128`, so it always
    /// fits. `new` does not enforce that range, so the conversion still has to be checked.
    #[test]
    fn after_swap_rejects_a_take_that_does_not_fit_int128() {
        let params = AfterSwapParameters {
            delta: BalanceDelta::new(I128::ZERO, I128::MAX),
            ..after_swap_params(true, -1_000, 0, 0)
        };

        let error = handler(u16::MAX, u16::MAX)
            .after_swap(params, None, None)
            .expect_err("655% of int128::MAX cannot be returned as an int128");

        assert!(matches!(error, SimulationError::FatalError(_)), "{error:?}");
    }

    /// The largest take the contract can produce is 20% of the unspecified leg, which always
    /// fits, so the checked conversion never rejects a real swap.
    #[test]
    fn after_swap_accepts_the_largest_take_the_contract_can_produce() {
        let params = AfterSwapParameters {
            delta: BalanceDelta::new(I128::ZERO, I128::MAX),
            ..after_swap_params(true, -1_000, 0, 0)
        };

        let delta = handler(1_000, 1_000)
            .after_swap(params, None, None)
            .expect("20% of int128::MAX fits an int128");

        assert!(delta.result > I128::ZERO);
    }

    #[test]
    fn before_swap_is_rejected_because_the_hook_has_no_before_swap_permission() {
        let error = handler(100, 150)
            .before_swap(
                crate::evm::protocol::uniswap_v4::hooks::models::BeforeSwapParameters {
                    context: StateContext {
                        currency_0: Address::ZERO,
                        currency_1: Address::ZERO,
                        fees: UniswapV4Fees::new(0, 0, 0),
                        tick_spacing: 200,
                    },
                    sender: SENDER,
                    swap_params: SwapParams {
                        zero_for_one: true,
                        amount_specified: I256::MINUS_ONE,
                        sqrt_price_limit: U256::ZERO,
                    },
                    hook_data: Bytes::new(),
                },
                None,
                None,
            )
            .expect_err("bit 7 is not set on the hook address");

        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
    }

    #[test]
    fn spot_price_is_left_to_the_pool() {
        let token = tycho_common::models::token::Token::new(
            &Bytes::from([0u8; 20]),
            "T0",
            18,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        );

        let error = handler(100, 150)
            .spot_price(&token, &token)
            .expect_err("the handler defers to UniswapV4State");

        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
    }

    /// The take the pool asks for analytically is the one `_afterSwap` charges, and it does not
    /// depend on the direction: the hook charges the unspecified leg either way. A pool that
    /// charges nothing still answers, because "nothing" is a fee the handler knows.
    #[rstest]
    #[case::zero_for_one(true, 100, 150, 25_000)]
    #[case::one_for_zero(false, 100, 150, 25_000)]
    #[case::no_charge_still_answers(true, 0, 0, 0)]
    fn unspecified_fee_amount_reports_what_after_swap_takes(
        #[case] zero_for_one: bool,
        #[case] hook_fee_bps: u16,
        #[case] creator_tax_bps: u16,
        #[case] expected: u64,
    ) {
        let hook = handler(hook_fee_bps, creator_tax_bps);
        let unspecified = U256::from(1_000_003u64);

        let reported = hook
            .unspecified_fee_amount(unspecified, zero_for_one)
            .expect("bounded inputs never overflow");

        assert_eq!(reported, Some(U256::from(expected)));
        assert_eq!(
            reported,
            Some(
                hook.fee_and_tax(unspecified)
                    .expect("bounded inputs never overflow")
            )
        );
    }

    #[test]
    fn unspecified_fee_amount_propagates_an_overflowing_amount() {
        let error = handler(100, 150)
            .unspecified_fee_amount(U256::MAX, true)
            .expect_err("multiplying U256::MAX by 100 bps overflows");

        assert!(matches!(error, SimulationError::FatalError(_)), "{error:?}");
    }

    /// `get_limits` treats a `"not implemented"` recoverable error as "derive the limits from the
    /// pool", so the message has to carry that exact marker.
    #[test]
    fn get_amount_ranges_reports_not_implemented() {
        let error = handler(100, 150)
            .get_amount_ranges(Bytes::from([0u8; 20]), Bytes::from([1u8; 20]))
            .expect_err("the handler has no ranges of its own");

        let SimulationError::RecoverableError(message) = error else {
            panic!("expected a recoverable error, got {error:?}");
        };
        assert!(message.contains("not implemented"), "{message}");
    }

    #[test]
    fn fee_is_the_combined_bps_as_a_fraction() {
        let fee = handler(100, 150)
            .fee(
                &crate::evm::protocol::uniswap_v4::state::UniswapV4State::new(
                    0,
                    U256::from(1),
                    UniswapV4Fees::new(0, 0, 0),
                    0,
                    1,
                    Vec::new(),
                )
                .expect("a bare pool state builds"),
                SwapParams {
                    zero_for_one: true,
                    amount_specified: I256::MINUS_ONE,
                    sqrt_price_limit: U256::ZERO,
                },
            )
            .expect("the combined bps are always known");

        assert!((fee - 0.025).abs() < f64::EPSILON, "{fee}");
    }

    /// The per-pool bps are frozen by `registerPool`, and `afterSwap` only moves accounting
    /// balances, so no state delta can change a quote.
    #[test]
    fn delta_transition_leaves_the_handler_unchanged() {
        let mut updated = handler(100, 150);
        let original = updated.clone();
        let delta = ProtocolStateDelta {
            component_id: "pons".to_string(),
            updated_attributes: [
                ("pons_hook_fee_bps".to_string(), Bytes::from(vec![0x03, 0xe8])),
                ("pons_creator_tax_bps".to_string(), Bytes::from(vec![0x03, 0xe8])),
            ]
            .into_iter()
            .collect(),
            deleted_attributes: Default::default(),
        };

        updated
            .delta_transition(delta, &Default::default(), &Default::default())
            .expect("the handler holds no mutable state");

        assert!(updated.is_equal(&original));
    }

    #[rstest]
    #[case::same(PONS_V2_HOOK_ROBINHOOD, 100, 150, true)]
    #[case::other_address(Address::ZERO, 100, 150, false)]
    #[case::other_hook_fee(PONS_V2_HOOK_ROBINHOOD, 101, 150, false)]
    #[case::other_creator_tax(PONS_V2_HOOK_ROBINHOOD, 100, 151, false)]
    fn is_equal_compares_the_address_and_both_bps(
        #[case] address: Address,
        #[case] hook_fee_bps: u16,
        #[case] creator_tax_bps: u16,
        #[case] expected: bool,
    ) {
        let other = PonsV2HookHandler::new(address, hook_fee_bps, creator_tax_bps);

        assert_eq!(handler(100, 150).is_equal(&other), expected);
    }

    #[derive(Deserialize)]
    struct RecordedSwaps {
        swaps: Vec<RecordedSwap>,
    }

    #[derive(Deserialize)]
    struct RecordedSwap {
        label: String,
        hook_fee_bps: u16,
        creator_tax_bps: u16,
        pre_state: RecordedPreState,
        position: RecordedPosition,
        swap: RecordedSwapLog,
        hook_fee: RecordedHookFee,
    }

    #[derive(Deserialize)]
    struct RecordedPreState {
        liquidity: String,
    }

    /// The pool's only liquidity position, which Task 8 rebuilds the tick list from.
    #[derive(Deserialize)]
    struct RecordedPosition {
        tick_lower: i32,
        tick_upper: i32,
        liquidity: String,
    }

    #[derive(Deserialize)]
    struct RecordedSwapLog {
        zero_for_one: bool,
        exact_input: bool,
        amount0: String,
        amount1: String,
    }

    #[derive(Deserialize)]
    struct RecordedHookFee {
        unspecified_amount: String,
        fee_amount: String,
        tax_amount: String,
    }

    fn recorded_swaps() -> Vec<RecordedSwap> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/assets/hooks/pons_v2/recorded_swaps.json");
        let raw =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str::<RecordedSwaps>(&raw)
            .expect("the recorded swap fixture should parse")
            .swaps
    }

    /// Task 8 rebuilds each pool from this fixture alone, so the position it would build the
    /// tick list from has to be the full-range one the pool actually holds, and the only one:
    /// its liquidity is therefore the pool's whole active liquidity before the recorded swap.
    #[test]
    fn every_recorded_swap_carries_its_pool_s_only_full_range_position() {
        for recorded in recorded_swaps() {
            assert_eq!(recorded.position.tick_lower, -887_200, "{}", recorded.label);
            assert_eq!(recorded.position.tick_upper, 887_200, "{}", recorded.label);
            assert_eq!(
                recorded.position.liquidity, recorded.pre_state.liquidity,
                "{}: the position is the pool's only one, so it holds all the active liquidity",
                recorded.label
            );
        }
    }

    /// Every recorded swap is a real Robinhood transaction: the `HookFeeCollected` amounts it
    /// emitted must come back out of `after_swap` to the wei.
    #[test]
    fn after_swap_reproduces_every_recorded_hook_fee() {
        let swaps = recorded_swaps();
        assert!(swaps.len() >= 6, "the fixture should cover both directions on several pools");

        for recorded in swaps {
            let parse = |s: &str| {
                s.parse::<i128>()
                    .expect("recorded amounts fit i128")
            };
            let amount0 = parse(&recorded.swap.amount0);
            let amount1 = parse(&recorded.swap.amount1);
            let unspecified = recorded
                .hook_fee
                .unspecified_amount
                .parse::<u128>()
                .expect("recorded amounts fit u128");
            let fee = recorded
                .hook_fee
                .fee_amount
                .parse::<u128>()
                .expect("recorded amounts fit u128");
            let tax = recorded
                .hook_fee
                .tax_amount
                .parse::<u128>()
                .expect("recorded amounts fit u128");

            assert_eq!(
                fee,
                unspecified * u128::from(recorded.hook_fee_bps) / 10_000,
                "{}: recorded fee is not floor(u * hookFeeBps / 10_000)",
                recorded.label
            );
            assert_eq!(
                tax,
                unspecified * u128::from(recorded.creator_tax_bps) / 10_000,
                "{}: recorded tax is not floor(u * creatorTaxBps / 10_000)",
                recorded.label
            );

            let amount_specified = if recorded.swap.exact_input { -1_000 } else { 1_000 };
            let params =
                after_swap_params(recorded.swap.zero_for_one, amount_specified, amount0, amount1);

            let delta = handler(recorded.hook_fee_bps, recorded.creator_tax_bps)
                .after_swap(params, None, None)
                .unwrap_or_else(|e| panic!("{}: {e:?}", recorded.label));

            assert_eq!(
                delta.result,
                I128::try_from(fee + tax).expect("recorded totals fit I128"),
                "{}: after_swap does not reproduce feeAmount + taxAmount",
                recorded.label
            );
        }
    }
}
