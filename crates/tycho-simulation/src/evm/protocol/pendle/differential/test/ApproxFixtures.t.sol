// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {MarketMathCore, MarketState, MarketPreCompute} from "pendle/core/Market/MarketMathCore.sol";
import {PYIndex} from "pendle/core/StandardizedYield/PYIndex.sol";
import {
    MarketApproxPtInLibOnchain,
    MarketApproxPtOutLibOnchain
} from "pendle/router/math/MarketApproxLibOnchain.sol";
import {MarketApproxEstimateLib} from "pendle/router/math/MarketApproxEstimateLib.sol";

/// Fixtures for the router's own approximation, which is what actually executes.
///
/// The market's primitives are exact-PT in both directions, so an exact-SY-in quote has to be
/// inverted. The contract does that by bounded search, and `ActionSwapPTV3.swapExactSyForPt`
/// delegates to this on-chain variant whenever the caller passes no off-chain guess — which is
/// what a router integration does.
///
/// The **iteration count** is recorded alongside the result on purpose. Two implementations can
/// converge to the same answer by different routes; only the same route guarantees that a quote
/// equals what the executor will produce for every input, rather than for the ones that happened
/// to be sampled.
contract ApproxFixtures is Test {
    using MarketApproxPtOutLibOnchain for MarketState;
    using MarketApproxPtInLibOnchain for MarketState;
    using MarketApproxEstimateLib for MarketState;

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

    function reUsdMarket() internal pure returns (MarketState memory m) {
        m.totalPt = 1_000_000_000_000;
        m.totalSy = 900_000_000_000_000_000_000_000;
        m.totalLp = 500_000_000_000;
        m.treasury = address(0x8270400d528c34e1596EF367eeDEc99080A1b592);
        m.scalarRoot = 40_000_000_000_000_000_000;
        m.expiry = 1_830_124_800;
        m.lnFeeRateRoot = 499_875_041_000_000;
        m.reserveFeePercent = 80;
        m.lastLnImpliedRate = 50_000_000_000_000_000;
    }

    uint256 constant REUSD_INDEX = 1_095_830;

    function test_writeApproxFixtures() public {
        string[] memory rows = new string[](48);
        uint256 n = 0;

        uint256[3] memory times = [uint256(1_700_000_000), 1_780_000_000, 1_820_000_000];

        // Sizes spanning six orders of magnitude, so the search starts inside its initial ±5%
        // window for some and has to extend the range for others — that is where the three-stage
        // state machine actually differs from a plain bisection.
        uint256[5] memory wstEthIn = [
            uint256(1_000_000_000_000_000), // 0.001 SY
            1_000_000_000_000_000_000, // 1 SY
            10_000_000_000_000_000_000, // 10 SY
            25_000_000_000_000_000_000, // 25 SY
            40_000_000_000_000_000_000 // 40 SY; the market tops out near 68 PT out
        ];
        // Buying YT is levered: one unit of SY buys roughly `1 / (1 - 1/impliedRate)` units of YT,
        // which on this market is about fifty. So the YT direction runs out of depth at a far
        // smaller SY input than the PT direction, and needs its own grid rather than a shared one.
        uint256[5] memory wstEthYtIn = [
            uint256(1_000_000_000_000_000), // 0.001 SY
            100_000_000_000_000_000, // 0.1 SY
            1_000_000_000_000_000_000, // 1 SY
            5_000_000_000_000_000_000, // 5 SY
            20_000_000_000_000_000_000 // 20 SY; the soft bound bites near 27
        ];

        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < wstEthIn.length; k++) {
                rows[n++] = ptRow("wsteth", wstEthMarket(), WSTETH_INDEX, times[t], wstEthIn[k]);
                rows[n++] = ytRow("wsteth", wstEthMarket(), WSTETH_INDEX, times[t], wstEthYtIn[k]);
            }
        }

        uint256[3] memory reUsdIn =
            [uint256(1_000_000_000_000_000_000), 1_000_000_000_000_000_000_000, 10_000_000_000_000_000_000_000];
        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < reUsdIn.length; k++) {
                rows[n++] = ptRow("reusd", reUsdMarket(), REUSD_INDEX, times[t], reUsdIn[k]);
                rows[n++] = ytRow("reusd", reUsdMarket(), REUSD_INDEX, times[t], reUsdIn[k]);
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/approx.json", string.concat('{"cases":[', body, "]}"));
    }

    function ptRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        uint256 exactSyIn
    ) internal view returns (string memory) {
        (uint256 netPtOut, uint256 netSyFee, uint256 iters) =
            MarketApproxPtOutLibOnchain.approxSwapExactSyForPtOnchain(
                market, PYIndex.wrap(index), exactSyIn, blockTime
            );
        uint256 estimate = market.estimateSwapExactSyForPt(PYIndex.wrap(index), blockTime, exactSyIn);
        return approxJson(label, "sy_for_pt", blockTime, exactSyIn, estimate, netPtOut, netSyFee, iters);
    }

    function ytRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime,
        uint256 exactSyIn
    ) internal view returns (string memory) {
        (uint256 netYtOut, uint256 netSyFee, uint256 iters) =
            MarketApproxPtInLibOnchain.approxSwapExactSyForYtOnchain(
                market, PYIndex.wrap(index), exactSyIn, blockTime
            );
        uint256 estimate = market.estimateSwapExactSyForYt(PYIndex.wrap(index), blockTime, exactSyIn);
        return approxJson(label, "sy_for_yt", blockTime, exactSyIn, estimate, netYtOut, netSyFee, iters);
    }

    function approxJson(
        string memory label,
        string memory direction,
        uint256 blockTime,
        uint256 exactSyIn,
        uint256 estimate,
        uint256 amountOut,
        uint256 netSyFee,
        uint256 iterations
    ) internal pure returns (string memory) {
        return string.concat(
            '{"market":"', label,
            '","direction":"', direction,
            '","block_time":"', vm.toString(blockTime),
            '","exact_sy_in":"', vm.toString(exactSyIn),
            '","estimate":"', vm.toString(estimate),
            '","amount_out":"', vm.toString(amountOut),
            '","net_sy_fee":"', vm.toString(netSyFee),
            '","iterations":"', vm.toString(iterations),
            '"}'
        );
    }

    /// The boundary between a size that fills and one that reverts, swept for both legs.
    ///
    /// This is where a search that converges differently would first disagree, so every size is
    /// recorded with its outcome — filled, or reverted and with which message — and the Rust port
    /// is asserted against both. Sweeping only sizes that fill would leave the interesting half
    /// untested.
    function test_writeApproxBoundaryFixtures() public {
        string[] memory rows = new string[](40);
        uint256 n = 0;

        uint256[6] memory sizes = [
            uint256(20_000_000_000_000_000_000),
            40_000_000_000_000_000_000,
            60_000_000_000_000_000_000,
            100_000_000_000_000_000_000,
            300_000_000_000_000_000_000,
            500_000_000_000_000_000_000
        ];
        uint256[2] memory times = [uint256(1_700_000_000), 1_820_000_000];

        for (uint256 t = 0; t < times.length; t++) {
            for (uint256 k = 0; k < sizes.length; k++) {
                rows[n++] = boundaryRow("sy_for_pt", times[t], sizes[k]);
                rows[n++] = boundaryRow("sy_for_yt", times[t], sizes[k]);
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile(
            "../tests/fixtures/approx_boundary.json", string.concat('{"cases":[', body, "]}")
        );
    }

    function boundaryRow(string memory direction, uint256 blockTime, uint256 exactSyIn)
        internal
        returns (string memory)
    {
        string memory outcome;
        string memory amount = "0";
        bool isPt = keccak256(bytes(direction)) == keccak256(bytes("sy_for_pt"));
        if (isPt) {
            try this.approxPt(wstEthMarket(), WSTETH_INDEX, exactSyIn, blockTime) returns (
                uint256 out, uint256, uint256
            ) {
                outcome = "ok";
                amount = vm.toString(out);
            } catch Error(string memory reason) {
                outcome = reason;
            }
        } else {
            try this.approxYt(wstEthMarket(), WSTETH_INDEX, exactSyIn, blockTime) returns (
                uint256 out, uint256, uint256
            ) {
                outcome = "ok";
                amount = vm.toString(out);
            } catch Error(string memory reason) {
                outcome = reason;
            }
        }
        return string.concat(
            '{"direction":"', direction,
            '","block_time":"', vm.toString(blockTime),
            '","exact_sy_in":"', vm.toString(exactSyIn),
            '","outcome":"', outcome,
            '","amount_out":"', amount,
            '"}'
        );
    }

    function approxPt(
        MarketState memory market,
        uint256 index,
        uint256 exactSyIn,
        uint256 blockTime
    ) external pure returns (uint256, uint256, uint256) {
        return MarketApproxPtOutLibOnchain.approxSwapExactSyForPtOnchain(
            market, PYIndex.wrap(index), exactSyIn, blockTime
        );
    }

    function approxYt(
        MarketState memory market,
        uint256 index,
        uint256 exactSyIn,
        uint256 blockTime
    ) external pure returns (uint256, uint256, uint256) {
        return MarketApproxPtInLibOnchain.approxSwapExactSyForYtOnchain(
            market, PYIndex.wrap(index), exactSyIn, blockTime
        );
    }

    /// The two depth bounds the router enforces. Recorded here rather than derived, because a
    /// first-principles reading of the brief gets both of them wrong: `calcMaxPtOut` keeps only
    /// 99.9% of the theoretical maximum, and the PT-in bound is the proportion cap rather than the
    /// reserve.
    function test_writeLimitFixtures() public {
        string[] memory rows = new string[](8);
        uint256 n = 0;
        uint256[3] memory times = [uint256(1_700_000_000), 1_780_000_000, 1_820_000_000];

        for (uint256 t = 0; t < times.length; t++) {
            rows[n++] = limitRow("wsteth", wstEthMarket(), WSTETH_INDEX, times[t]);
            rows[n++] = limitRow("reusd", reUsdMarket(), REUSD_INDEX, times[t]);
        }

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, rows[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/limits.json", string.concat('{"limits":[', body, "]}"));
    }

    function limitRow(
        string memory label,
        MarketState memory market,
        uint256 index,
        uint256 blockTime
    ) internal pure returns (string memory) {
        MarketPreCompute memory comp =
            MarketMathCore.getMarketPreCompute(market, PYIndex.wrap(index), blockTime);
        return string.concat(
            '{"market":"', label,
            '","block_time":"', vm.toString(blockTime),
            '","max_pt_out":"',
            vm.toString(MarketApproxPtOutLibOnchain.calcMaxPtOut(comp, market.totalPt)),
            '","soft_max_pt_in":"',
            vm.toString(MarketApproxPtInLibOnchain.calcSoftMaxPtIn(market, comp)),
            '"}'
        );
    }
}
