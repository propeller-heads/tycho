//! Reads what a Fluid DEX v2 swap depends on outside its pool through the DEX v2 resolver's
//! `getTokenBaseState`, run against the VM database.
//!
//! The substreams registers this call with the Dynamic Contract Indexer for every pool token, so
//! the database holds every contract and slot it reads: the dex base state and Fluid Liquidity.
use std::fmt::Debug;

use alloy::{primitives::U256, sol, sol_types::SolCall};
use revm::DatabaseRef;
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    simulation::{SimulationEngine, SimulationParameters},
};

sol! {
    #[derive(Default)]
    struct RateDataV1Params {
        address token;
        uint256 kink;
        uint256 rateAtUtilizationZero;
        uint256 rateAtUtilizationKink;
        uint256 rateAtUtilizationMax;
    }

    #[derive(Default)]
    struct RateDataV2Params {
        address token;
        uint256 kink1;
        uint256 kink2;
        uint256 rateAtUtilizationZero;
        uint256 rateAtUtilizationKink1;
        uint256 rateAtUtilizationKink2;
        uint256 rateAtUtilizationMax;
    }

    #[derive(Default)]
    struct RateData {
        uint256 version;
        RateDataV1Params rateDataV1;
        RateDataV2Params rateDataV2;
    }

    #[derive(Default)]
    struct OverallTokenData {
        uint256 borrowRate;
        uint256 supplyRate;
        uint256 fee;
        bool isSupplyPaused;
        bool isWithdrawPaused;
        bool isBorrowPaused;
        bool isPaybackPaused;
        bool isPaused;
        uint256 lastStoredUtilization;
        uint256 storageUpdateThreshold;
        uint256 lastUpdateTimestamp;
        uint256 supplyExchangePrice;
        uint256 borrowExchangePrice;
        uint256 supplyRawInterest;
        uint256 supplyInterestFree;
        uint256 borrowRawInterest;
        uint256 borrowInterestFree;
        uint256 totalSupply;
        uint256 totalBorrow;
        uint256 revenue;
        uint256 maxUtilization;
        RateData rateData;
    }

    #[derive(Default)]
    struct UserSupplyData {
        bool modeWithInterest;
        bool isSupplyPaused;
        bool isWithdrawPaused;
        bool isPaused;
        uint256 supply;
        uint256 withdrawalLimit;
        uint256 lastUpdateTimestamp;
        uint256 expandPercent;
        uint256 expandDuration;
        uint256 baseWithdrawalLimit;
        uint256 withdrawableUntilLimit;
        uint256 withdrawable;
        uint256 decayEndTimestamp;
        uint256 decayAmount;
    }

    #[derive(Default)]
    struct UserBorrowData {
        bool modeWithInterest;
        bool isBorrowPaused;
        bool isPaybackPaused;
        bool isPaused;
        uint256 borrow;
        uint256 borrowLimit;
        uint256 lastUpdateTimestamp;
        uint256 expandPercent;
        uint256 expandDuration;
        uint256 baseBorrowLimit;
        uint256 maxBorrowLimit;
        uint256 borrowableUntilLimit;
        uint256 borrowable;
        uint256 borrowLimitUtilization;
    }

    #[derive(Default)]
    struct TokenBaseState {
        uint256 floatBalance;
        uint256 maxCap;
        int256 unaccountedBorrow;
        UserSupplyData dexSupplyAtLiquidity;
        UserBorrowData dexBorrowAtLiquidity;
        OverallTokenData liquidityTokenData;
        uint256 availableForSwapOut;
        uint256 availableForSwapOutWithdraw;
        uint256 availableForSwapOutBorrow;
    }

    function getTokenBaseState(address token_) public view returns (TokenBaseState memory);

    function readFromStorage(bytes32 slot_) public view returns (uint256 result_);
}

/// Fluid Liquidity's `_status` slot. `changeStatus(2)` pauses every Liquidity operation by setting
/// it to the reentrancy guard's "entered" value.
const LIQUIDITY_STATUS_SLOT: u64 = 1;
const LIQUIDITY_PAUSED_STATUS: u64 = 2;

/// What the dex can do with one token at Fluid Liquidity, in token units.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenLiquidity {
    /// Liquidity supply exchange price (1e12 precision), which D3 pool math runs on.
    pub supply_exchange_price: U256,
    /// Liquidity borrow exchange price (1e12 precision), which D4 pool math runs on.
    pub borrow_exchange_price: U256,
    /// The dex's own balance of the token, which serves settles without calling Liquidity.
    pub float_balance: U256,
    /// Largest payout of a supply leg (D3 swap out).
    pub available_for_swap_out_withdraw: U256,
    /// Largest payout of a borrow leg (D4 swap out), fees not included.
    pub available_for_swap_out_borrow: U256,
    /// Whether Liquidity rejects each leg for this token, at the token or for the dex.
    pub supply_paused: bool,
    pub withdraw_paused: bool,
    pub borrow_paused: bool,
    pub payback_paused: bool,
}

impl From<TokenBaseState> for TokenLiquidity {
    /// Any token-level pause flag stops every leg of the token. The deployed Liquidity pauses a
    /// token with one bit that makes `operate` revert for every operation, while the resolver
    /// decodes that bit as a payback pause only. The dex's own supply and borrow positions are
    /// paused per side, and the resolver decodes those as they are enforced.
    fn from(state: TokenBaseState) -> Self {
        let token = &state.liquidityTokenData;
        let supply = &state.dexSupplyAtLiquidity;
        let borrow = &state.dexBorrowAtLiquidity;
        let token_paused = token.isPaused ||
            token.isSupplyPaused ||
            token.isWithdrawPaused ||
            token.isBorrowPaused ||
            token.isPaybackPaused;
        Self {
            supply_exchange_price: token.supplyExchangePrice,
            borrow_exchange_price: token.borrowExchangePrice,
            float_balance: state.floatBalance,
            available_for_swap_out_withdraw: state.availableForSwapOutWithdraw,
            available_for_swap_out_borrow: state.availableForSwapOutBorrow,
            supply_paused: token_paused || supply.isPaused || supply.isSupplyPaused,
            withdraw_paused: token_paused || supply.isPaused || supply.isWithdrawPaused,
            borrow_paused: token_paused || borrow.isPaused || borrow.isBorrowPaused,
            payback_paused: token_paused || borrow.isPaused || borrow.isPaybackPaused,
        }
    }
}

/// Calls `getTokenBaseState(token)` on `resolver` against the engine's current block.
///
/// `token` is Fluid's address for the token: the native token is 0xEeee…EEeE.
pub(super) fn fetch_token_liquidity<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    resolver: alloy::primitives::Address,
    token: alloy::primitives::Address,
) -> Result<TokenLiquidity, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let params = SimulationParameters {
        caller: alloy::primitives::Address::ZERO,
        to: resolver,
        data: getTokenBaseStateCall { token_: token }.abi_encode(),
        ..Default::default()
    };
    let result = engine.simulate(&params).map_err(|e| {
        SimulationError::RecoverableError(format!(
            "Fluid v2 resolver call getTokenBaseState({token}) failed: {e}"
        ))
    })?;
    decode_token_liquidity(&result.result)
}

/// Whether Fluid Liquidity is paused as a whole, read from its `_status` against the engine's
/// current block.
pub(super) fn fetch_liquidity_paused<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    liquidity: alloy::primitives::Address,
) -> Result<bool, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let params = SimulationParameters {
        caller: alloy::primitives::Address::ZERO,
        to: liquidity,
        data: readFromStorageCall { slot_: U256::from(LIQUIDITY_STATUS_SLOT).into() }.abi_encode(),
        ..Default::default()
    };
    let result = engine.simulate(&params).map_err(|e| {
        SimulationError::RecoverableError(format!(
            "Fluid Liquidity {liquidity} call readFromStorage(_status) failed: {e}"
        ))
    })?;
    let status = readFromStorageCall::abi_decode_returns(&result.result).map_err(|e| {
        SimulationError::FatalError(format!(
            "Failed to decode Fluid Liquidity readFromStorage return data: {e}"
        ))
    })?;
    Ok(status == U256::from(LIQUIDITY_PAUSED_STATUS))
}

pub(super) fn decode_token_liquidity(bytes: &[u8]) -> Result<TokenLiquidity, SimulationError> {
    getTokenBaseStateCall::abi_decode_returns(bytes)
        .map(TokenLiquidity::from)
        .map_err(|e| {
            SimulationError::FatalError(format!(
                "Failed to decode Fluid v2 getTokenBaseState return data: {e}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn decode(state: TokenBaseState) -> TokenLiquidity {
        decode_token_liquidity(&getTokenBaseStateCall::abi_encode_returns(&state)).unwrap()
    }

    /// (supply, withdraw, borrow, payback) pause flags.
    fn pauses(liquidity: &TokenLiquidity) -> (bool, bool, bool, bool) {
        (
            liquidity.supply_paused,
            liquidity.withdraw_paused,
            liquidity.borrow_paused,
            liquidity.payback_paused,
        )
    }

    #[test]
    fn decodes_prices_and_payout_caps() {
        let state = TokenBaseState {
            floatBalance: U256::from(1u64),
            availableForSwapOutWithdraw: U256::from(2u64),
            availableForSwapOutBorrow: U256::from(3u64),
            liquidityTokenData: OverallTokenData {
                supplyExchangePrice: U256::from(4u64),
                borrowExchangePrice: U256::from(5u64),
                ..Default::default()
            },
            ..Default::default()
        };

        let liquidity = decode(state);

        assert_eq!(liquidity.float_balance, U256::from(1u64));
        assert_eq!(liquidity.available_for_swap_out_withdraw, U256::from(2u64));
        assert_eq!(liquidity.available_for_swap_out_borrow, U256::from(3u64));
        assert_eq!(liquidity.supply_exchange_price, U256::from(4u64));
        assert_eq!(liquidity.borrow_exchange_price, U256::from(5u64));
        assert_eq!(pauses(&liquidity), (false, false, false, false));
    }

    /// The deployed Liquidity's token pause reaches the resolver as a payback pause only.
    #[rstest]
    #[case::paused(|data: &mut OverallTokenData| data.isPaused = true)]
    #[case::payback_only(|data: &mut OverallTokenData| data.isPaybackPaused = true)]
    #[case::supply_only(|data: &mut OverallTokenData| data.isSupplyPaused = true)]
    fn token_pause_stops_every_leg(#[case] pause: fn(&mut OverallTokenData)) {
        let mut state = TokenBaseState::default();
        pause(&mut state.liquidityTokenData);

        assert_eq!(pauses(&decode(state)), (true, true, true, true));
    }

    #[rstest]
    #[case::supply(|s: &mut TokenBaseState| s.dexSupplyAtLiquidity.isSupplyPaused = true, (true, false, false, false))]
    #[case::withdraw(|s: &mut TokenBaseState| s.dexSupplyAtLiquidity.isWithdrawPaused = true, (false, true, false, false))]
    #[case::supply_position(|s: &mut TokenBaseState| s.dexSupplyAtLiquidity.isPaused = true, (true, true, false, false))]
    #[case::borrow(|s: &mut TokenBaseState| s.dexBorrowAtLiquidity.isBorrowPaused = true, (false, false, true, false))]
    #[case::payback(|s: &mut TokenBaseState| s.dexBorrowAtLiquidity.isPaybackPaused = true, (false, false, false, true))]
    #[case::borrow_position(|s: &mut TokenBaseState| s.dexBorrowAtLiquidity.isPaused = true, (false, false, true, true))]
    fn dex_position_pause_stops_its_side(
        #[case] pause: fn(&mut TokenBaseState),
        #[case] expected: (bool, bool, bool, bool),
    ) {
        let mut state = TokenBaseState::default();
        pause(&mut state);

        assert_eq!(pauses(&decode(state)), expected);
    }
}
