// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {MarketMathCore, MarketState, MarketPreCompute} from "pendle/core/Market/MarketMathCore.sol";
import {PYIndex} from "pendle/core/StandardizedYield/PYIndex.sol";
import {Errors} from "pendle/core/libraries/Errors.sol";

/// Writes the fixtures the Rust port of `MarketMathCore` is asserted against.
///
/// As with the LogExpMath fixtures, this calls the upstream library rather than reimplementing it.
/// The grid deliberately varies the two things a static reading of the brief would miss: the
/// **block timestamp**, which moves `rateScalar`, `rateAnchor` and `feeRate` on every quote, and the
/// **decimal gap** between SY and the accounting asset, which the PY index absorbs.
///
/// Run `./regenerate.sh` rather than invoking this directly.
contract MarketMathFixtures is Test {
    using MarketMathCore for MarketState;

    uint256 constant IMPLIED_RATE_TIME = 365 days;

    /// The brief's reference market: wstETH, expiry 30 Dec 2027, SY and accounting asset both 18
    /// decimals. Values as quoted, so a reviewer can tie the fixtures back to the source.
    function wstEthMarket() internal pure returns (MarketState memory m) {
        m.totalPt = 83_658_000_000_000_000_000;
        m.totalSy = 1_429_719_000_000_000_000_000;
        m.totalLp = 100_000_000_000_000_000_000;
        m.treasury = address(0x8270400d528c34e1596EF367eeDEc99080A1b592);
        m.scalarRoot = 86_364_560_000_000_000_000;
        m.expiry = 1_830_124_800;
        m.lnFeeRateRoot = 499_875_041_000_000;
        m.reserveFeePercent = 80;
        m.lastLnImpliedRate = 20_211_000_000_000_000;
    }

    uint256 constant WSTETH_INDEX = 1_241_884_000_000_000_000;

    /// A market on the other side of the decimal axis: SY has 18 decimals, the accounting asset
    /// has 6, and the index carries the 1e12 gap. `totalPt` is in asset units, `totalSy` is not.
    function reUsdMarket() internal pure returns (MarketState memory m) {
        m.totalPt = 1_000_000_000_000; // 1,000,000 units of a 6-decimal asset
        m.totalSy = 900_000_000_000_000_000_000_000; // 900,000 SY at 18 decimals
        m.totalLp = 500_000_000_000;
        m.treasury = address(0x8270400d528c34e1596EF367eeDEc99080A1b592);
        m.scalarRoot = 40_000_000_000_000_000_000;
        m.expiry = 1_830_124_800;
        m.lnFeeRateRoot = 499_875_041_000_000;
        m.reserveFeePercent = 80;
        m.lastLnImpliedRate = 50_000_000_000_000_000;
    }

    uint256 constant REUSD_INDEX = 1_095_830;

    function test_writeMarketFixtures() public {
        string[] memory rows = new string[](64);
        uint256 n = 0;

        // Timestamps spread across the market's life. The same state at two different times must
        // produce different quotes — that is the time-dependence the brief calls out, and a port
        // that ignores blockTime passes every static test while being wrong all day.
        uint256[4] memory times =
            [uint256(1_700_000_000), 1_780_000_000, 1_820_000_000, 1_830_124_799];

        // Both directions and several magnitudes. Positive is PT leaving the market to the
        // account (SY->PT); negative is PT arriving (PT->SY).
        int256[6] memory wstEthTrades = [
            int256(1_000_000_000_000_000), // 0.001 PT out
            int256(1_000_000_000_000_000_000), // 1 PT out
            int256(10_000_000_000_000_000_000), // 10 PT out
            -int256(1_000_000_000_000_000),
            -int256(1_000_000_000_000_000_000),
            -int256(50_000_000_000_000_000_000)
        ];

        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < wstEthTrades.length; k++) {
                rows[n++] = tradeRow("wsteth", wstEthMarket(), WSTETH_INDEX, times[t], wstEthTrades[k]);
            }
        }

        int256[4] memory reUsdTrades = [
            int256(1_000_000), // 1 unit of a 6-decimal asset
            int256(1_000_000_000), // 1,000 units
            -int256(1_000_000),
            -int256(100_000_000_000)
        ];
        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < reUsdTrades.length; k++) {
                rows[n++] = tradeRow("reusd", reUsdMarket(), REUSD_INDEX, times[t], reUsdTrades[k]);
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/market.json", string.concat('{"trades":[', body, "]}"));
    }

    /// One row: every pre-computed value and every output of `calcTrade`, so a divergence is
    /// localised to a step rather than only showing up in the final amount.
    ///
    /// Split across three helpers because a single concatenation of this many fields exhausts the
    /// stack even with `via_ir`.
    function tradeRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        int256 netPtToAccount
    ) internal pure returns (string memory) {
        MarketPreCompute memory comp =
            MarketMathCore.getMarketPreCompute(market, PYIndex.wrap(index), blockTime);
        return string.concat(
            inputsJson(label, market, index, blockTime, netPtToAccount),
            preComputeJson(comp),
            outputsJson(market, comp, index, netPtToAccount)
        );
    }

    function inputsJson(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        int256 netPtToAccount
    ) internal pure returns (string memory) {
        return string.concat(
            '{"market":"', label,
            '","index":"', vm.toString(index),
            '","block_time":"', vm.toString(blockTime),
            '","total_pt":"', vm.toString(market.totalPt),
            '","total_sy":"', vm.toString(market.totalSy),
            '","net_pt_to_account":"', vm.toString(netPtToAccount),
            '"'
        );
    }

    function preComputeJson(MarketPreCompute memory comp) internal pure returns (string memory) {
        return string.concat(
            ',"rate_scalar":"', vm.toString(comp.rateScalar),
            '","total_asset":"', vm.toString(comp.totalAsset),
            '","rate_anchor":"', vm.toString(comp.rateAnchor),
            '","fee_rate":"', vm.toString(comp.feeRate),
            '"'
        );
    }

    function outputsJson(
        MarketState memory market,
        MarketPreCompute memory comp,
        uint256 index,
        int256 netPtToAccount
    ) internal pure returns (string memory) {
        (int256 netSyToAccount, int256 netSyFee, int256 netSyToReserve) =
            MarketMathCore.calcTrade(market, comp, PYIndex.wrap(index), netPtToAccount);
        return string.concat(
            ',"net_sy_to_account":"', vm.toString(netSyToAccount),
            '","net_sy_fee":"', vm.toString(netSyFee),
            '","net_sy_to_reserve":"', vm.toString(netSyToReserve),
            '"}'
        );
    }

    /// The failure modes the brief requires to surface as typed errors. Recorded by selector so
    /// the Rust port is pinned to fail on the same inputs, for the same reason.
    function test_writeRevertFixtures() public {
        string[] memory rows = new string[](8);
        uint256 n = 0;

        MarketState memory m = wstEthMarket();

        // At expiry, not after it: `isExpired` is `blockTime >= expiry`.
        rows[n++] = revertRow("expired_at_expiry", m, WSTETH_INDEX, m.expiry, 1e15);
        rows[n++] = revertRow("expired_after_expiry", m, WSTETH_INDEX, m.expiry + 1, 1e15);
        // More PT out than the market holds.
        rows[n++] = revertRow("pt_exceeds_reserve", m, WSTETH_INDEX, 1_800_000_000, m.totalPt);
        rows[n++] = revertRow("pt_far_exceeds_reserve", m, WSTETH_INDEX, 1_800_000_000, m.totalPt * 2);
        // Selling enough PT into the market to push the proportion past 96%.
        rows[n++] = revertRow("proportion_cap", m, WSTETH_INDEX, 1_800_000_000, -m.totalPt * 40);

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/market_reverts.json", string.concat('{"reverts":[', body, "]}"));
    }

    function revertRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        int256 netPtToAccount
    ) internal returns (string memory) {
        string memory name;
        try this.executeTrade(market, index, netPtToAccount, blockTime) {
            revert(string.concat("expected a revert for ", label));
        } catch (bytes memory reason) {
            name = errorName(bytes4(reason));
        }
        return string.concat(
            '{"case":"', label,
            '","block_time":"', vm.toString(blockTime),
            '","net_pt_to_account":"', vm.toString(netPtToAccount),
            '","error":"', name,
            '"}'
        );
    }

    function errorName(bytes4 selector) internal pure returns (string memory) {
        if (selector == Errors.MarketExpired.selector) return "MarketExpired";
        if (selector == Errors.MarketInsufficientPtForTrade.selector) {
            return "MarketInsufficientPtForTrade";
        }
        if (selector == Errors.MarketProportionTooHigh.selector) return "MarketProportionTooHigh";
        if (selector == Errors.MarketExchangeRateBelowOne.selector) {
            return "MarketExchangeRateBelowOne";
        }
        if (selector == Errors.MarketZeroTotalPtOrTotalAsset.selector) {
            return "MarketZeroTotalPtOrTotalAsset";
        }
        if (selector == Errors.MarketProportionMustNotEqualOne.selector) {
            return "MarketProportionMustNotEqualOne";
        }
        if (selector == Errors.MarketRateScalarBelowZero.selector) return "MarketRateScalarBelowZero";
        return "Unknown";
    }

    /// `external` so `try/catch` can reach it; `executeTradeCore` mutates its `memory` argument,
    /// which is exactly what the quote path must not do to indexed state.
    function executeTrade(
        MarketState memory market,
        uint256 index,
        int256 netPtToAccount,
        uint256 blockTime
    ) external pure returns (int256, int256, int256) {
        return MarketMathCore.executeTradeCore(market, PYIndex.wrap(index), netPtToAccount, blockTime);
    }
}
