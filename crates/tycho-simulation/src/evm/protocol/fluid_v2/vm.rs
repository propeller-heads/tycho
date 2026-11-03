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
    struct RateDataV1Params {
        address token;
        uint256 kink;
        uint256 rateAtUtilizationZero;
        uint256 rateAtUtilizationKink;
        uint256 rateAtUtilizationMax;
    }

    struct RateDataV2Params {
        address token;
        uint256 kink1;
        uint256 kink2;
        uint256 rateAtUtilizationZero;
        uint256 rateAtUtilizationKink1;
        uint256 rateAtUtilizationKink2;
        uint256 rateAtUtilizationMax;
    }

    struct RateData {
        uint256 version;
        RateDataV1Params rateDataV1;
        RateDataV2Params rateDataV2;
    }

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
}

/// What the dex can do with one token at Fluid Liquidity, in token units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    fn from(state: TokenBaseState) -> Self {
        let token = &state.liquidityTokenData;
        let supply = &state.dexSupplyAtLiquidity;
        let borrow = &state.dexBorrowAtLiquidity;
        Self {
            supply_exchange_price: token.supplyExchangePrice,
            borrow_exchange_price: token.borrowExchangePrice,
            float_balance: state.floatBalance,
            available_for_swap_out_withdraw: state.availableForSwapOutWithdraw,
            available_for_swap_out_borrow: state.availableForSwapOutBorrow,
            supply_paused: token.isPaused ||
                token.isSupplyPaused ||
                supply.isPaused ||
                supply.isSupplyPaused,
            withdraw_paused: token.isPaused ||
                token.isWithdrawPaused ||
                supply.isPaused ||
                supply.isWithdrawPaused,
            borrow_paused: token.isPaused ||
                token.isBorrowPaused ||
                borrow.isPaused ||
                borrow.isBorrowPaused,
            payback_paused: token.isPaused ||
                token.isPaybackPaused ||
                borrow.isPaused ||
                borrow.isPaybackPaused,
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

pub(super) fn decode_token_liquidity(bytes: &[u8]) -> Result<TokenLiquidity, SimulationError> {
    getTokenBaseStateCall::abi_decode_returns(bytes)
        .map(TokenLiquidity::from)
        .map_err(|e| {
            SimulationError::FatalError(format!(
                "Failed to decode Fluid v2 getTokenBaseState return data: {e}"
            ))
        })
}
