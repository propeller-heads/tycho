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
        string[] memory rows = new string[](96);
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

        n = feeVariantRows(rows, n);

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/market.json", string.concat('{"trades":[', body, "]}"));
    }

    /// The fee axis, which the two reference markets do not vary: both carry the same
    /// `lnFeeRateRoot` and the same 80% reserve split, so every row above holds the fee
    /// configuration constant while varying everything else.
    ///
    /// The two ends of the reserve split are what make it an axis rather than a single ratio — at
    /// 0 the treasury takes nothing and `netSyToReserve` must be zero, at 100 it takes the whole
    /// fee and `netSyToReserve` must equal `netSyFee`. A port that divided by the wrong constant,
    /// or applied the split to the wrong side, agrees with the contract at 80 for one operand
    /// ordering and disagrees at both ends.
    ///
    /// A zero fee root is included because it collapses `feeRate` to exactly `exp(0) = 1e18`, and
    /// the two branches of `calcTrade` treat that value asymmetrically: buying PT computes a fee of
    /// `preFeeAsset * (1 - feeRate)`, which is zero, while selling divides *by* `feeRate`. A port
    /// that mirrored the branches would still return zero on one side and something else on the
    /// other.
    function feeVariantRows(string[] memory rows, uint256 n) internal pure returns (uint256) {
        // (lnFeeRateRoot, reserveFeePercent)
        uint256[2][5] memory feeConfigs = [
            [uint256(499_875_041_000_000), 0], // the reference root, no treasury cut
            [uint256(499_875_041_000_000), 100], // the reference root, the whole fee to treasury
            [uint256(999_750_000_000_000), 80], // roughly twice the reference root
            [uint256(999_750_000_000_000), 50], // an even split
            [uint256(0), 80] // no fee at all: feeRate collapses to one
        ];
        uint256[2] memory times = [uint256(1_700_000_000), 1_820_000_000];
        int256[2] memory trades =
            [int256(1_000_000_000_000_000_000), -int256(1_000_000_000_000_000_000)];

        for (uint256 f = 0; f < feeConfigs.length; f++) {
            for (uint256 t = 0; t < times.length; t++) {
                for (uint256 k = 0; k < trades.length; k++) {
                    MarketState memory m = wstEthMarket();
                    m.lnFeeRateRoot = feeConfigs[f][0];
                    m.reserveFeePercent = feeConfigs[f][1];
                    rows[n++] = tradeRow("wsteth", m, WSTETH_INDEX, times[t], trades[k]);
                }
            }
        }
        return n;
    }

    /// One row: every pre-computed value, every output of `calcTrade`, and the market state the
    /// trade leaves behind, so a divergence is localised to a step rather than only showing up in
    /// the final amount.
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
        string memory head = string.concat(
            inputsJson(label, market, index, blockTime, netPtToAccount),
            feeConfigJson(market),
            preComputeJson(comp)
        );
        string memory outputs = outputsJson(market, comp, index, netPtToAccount);
        // Last, because it mutates `market` — every field above is read from the pre-trade state.
        return string.concat(head, outputs, stateWriteJson(market, index, blockTime, netPtToAccount));
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

    /// The fee configuration, in its own helper because folding it into `inputsJson` puts that
    /// concatenation two slots over the stack limit even with `via_ir`.
    function feeConfigJson(MarketState memory market) internal pure returns (string memory) {
        return string.concat(
            ',"ln_fee_rate_root":"', vm.toString(market.lnFeeRateRoot),
            '","reserve_fee_percent":"', vm.toString(market.reserveFeePercent),
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
            '"'
        );
    }

    /// The state the trade leaves behind: `executeTradeCore` mutates its `MarketState` argument,
    /// and what it writes there is what the *next* trade is priced against.
    ///
    /// Recorded per row rather than derived, because the reserves and the implied rate move
    /// together and none of the three follows from the quote alone: `totalSy` moves by the
    /// treasury's cut as well as the trader's amount, and `lastLnImpliedRate` is taken at the new
    /// reserves but at the *trade's* anchor and scalar, not at ones recomputed from them.
    ///
    /// Mutates `market`, so it must be called after every field read from the pre-trade state.
    function stateWriteJson(
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        int256 netPtToAccount
    ) internal pure returns (string memory) {
        MarketMathCore.executeTradeCore(market, PYIndex.wrap(index), netPtToAccount, blockTime);
        return string.concat(
            ',"post_total_pt":"', vm.toString(market.totalPt),
            '","post_total_sy":"', vm.toString(market.totalSy),
            '","post_last_ln_implied_rate":"', vm.toString(market.lastLnImpliedRate),
            '"}'
        );
    }

    /// `_getLnImpliedRate`: the rate the market carries *after* a trade, which is the input to the
    /// next trade's anchor.
    ///
    /// It is not on the quote path itself, so it is not covered by any of the rows above — but it
    /// is what `lastLnImpliedRate` becomes, and every subsequent quote is anchored on it. The
    /// reserves are perturbed away from the market's own so the grid covers post-trade states
    /// rather than only the state the fixtures start from.
    function test_writeImpliedRateFixtures() public {
        string[] memory rows = new string[](32);
        uint256 n = 0;

        uint256[3] memory times =
            [uint256(1_700_000_000), 1_780_000_000, 1_830_124_799];
        // Multipliers on `totalPt`, in percent: the proportion moves with the trade, and the logit
        // is what the implied rate is taken from.
        uint256[4] memory ptScale = [uint256(100), 90, 110, 150];

        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < ptScale.length; k++) {
                rows[n++] =
                    impliedRateRow("wsteth", wstEthMarket(), WSTETH_INDEX, times[t], ptScale[k]);
                rows[n++] =
                    impliedRateRow("reusd", reUsdMarket(), REUSD_INDEX, times[t], ptScale[k]);
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile(
            "../tests/fixtures/implied_rate.json", string.concat('{"cases":[', body, "]}")
        );
    }

    function impliedRateRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        uint256 ptScalePercent
    ) internal pure returns (string memory) {
        MarketPreCompute memory comp =
            MarketMathCore.getMarketPreCompute(market, PYIndex.wrap(index), blockTime);
        int256 totalPt = (market.totalPt * int256(ptScalePercent)) / 100;
        uint256 timeToExpiry = market.expiry - blockTime;
        uint256 lnImpliedRate = MarketMathCore._getLnImpliedRate(
            totalPt, comp.totalAsset, comp.rateScalar, comp.rateAnchor, timeToExpiry
        );
        return string.concat(
            '{"market":"', label,
            '","total_pt":"', vm.toString(totalPt),
            '","total_asset":"', vm.toString(comp.totalAsset),
            '","rate_scalar":"', vm.toString(comp.rateScalar),
            '","rate_anchor":"', vm.toString(comp.rateAnchor),
            '","time_to_expiry":"', vm.toString(timeToExpiry),
            '","ln_implied_rate":"', vm.toString(lnImpliedRate),
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
