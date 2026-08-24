// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {SYUtils} from "pendle/core/StandardizedYield/SYUtils.sol";
import {PYIndex, PYIndexLib} from "pendle/core/StandardizedYield/PYIndex.sol";

/// Writes the fixtures the Rust port of `SYUtils` and `PYIndexLib` is asserted against.
///
/// Two things make this the easiest place in the integration to ship a silently wrong quote, and
/// the grid is built around both:
///
/// - **The index is a raw uint, not a rate in 18 decimals.** It absorbs the decimal gap between SY
///   and the accounting asset, so the reUSD market's index reads `1_095_830`. Every case is
///   evaluated at that index as well as at the wstETH market's near-one index, and at `3` and `1`,
///   where the truncation is impossible to miss.
/// - **The rounding direction is chosen per call site**, so `Up` and `Down` are recorded for the
///   same operands rather than separately. They differ by exactly one wei on a remainder and not at
///   all without one, which is what makes a swapped call site detectable.
///
/// The signed variants take `abs()`, convert, then reapply the sign, so a negative amount truncates
/// toward zero rather than toward negative infinity. Both signs of the same magnitude are recorded
/// for exactly that reason.
///
/// Run `./regenerate.sh` rather than invoking this directly.
contract SYUtilsFixtures is Test {
    using PYIndexLib for PYIndex;

    string[] internal rows;

    /// wstETH: SY and accounting asset both 18 decimals, index just above one.
    uint256 constant WSTETH_INDEX = 1_241_884_000_000_000_000;
    /// reUSD: SY has 18 decimals, the asset has 6, and the index carries the 1e12 gap.
    uint256 constant REUSD_INDEX = 1_095_830;

    function indices() internal pure returns (uint256[5] memory ix) {
        ix[0] = WSTETH_INDEX;
        ix[1] = REUSD_INDEX;
        ix[2] = 1e18; // identity, where up and down must agree everywhere
        ix[3] = 3; // remainder on almost every input
        ix[4] = 1; // the extreme: one wei of index
    }

    function amounts() internal pure returns (uint256[8] memory a) {
        a[0] = 0;
        a[1] = 1;
        a[2] = 1_000_000; // one unit of a 6-decimal asset
        a[3] = 1e18; // one whole SY
        a[4] = 1_095_830;
        a[5] = 83_658_000_000_000_000_000; // the wstETH market's PT reserve
        a[6] = 1_429_719_000_000_000_000_000; // and its SY reserve
        a[7] = 999_999_999_999_999_999;
    }

    function test_writeSyUtilsFixtures() public {
        uint256[5] memory ix = indices();
        uint256[8] memory am = amounts();

        for (uint256 i = 0; i < ix.length; i++) {
            for (uint256 k = 0; k < am.length; k++) {
                unsignedRows(ix[i], am[k]);
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < rows.length; i++) {
            body = string.concat(body, rows[i], i + 1 == rows.length ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/sy_utils.json", string.concat('{"cases":[', body, "]}"));
    }

    /// All four unsigned conversions on the same operands, so the up/down pair is comparable row to
    /// row rather than only in aggregate.
    function unsignedRows(uint256 index, uint256 amount) internal {
        row("sy_to_asset", index, vm.toString(amount), vm.toString(SYUtils.syToAsset(index, amount)));
        row(
            "sy_to_asset_up",
            index,
            vm.toString(amount),
            vm.toString(SYUtils.syToAssetUp(index, amount))
        );
        row("asset_to_sy", index, vm.toString(amount), vm.toString(SYUtils.assetToSy(index, amount)));
        row(
            "asset_to_sy_up",
            index,
            vm.toString(amount),
            vm.toString(SYUtils.assetToSyUp(index, amount))
        );
    }

    /// The signed variants, both signs of each magnitude.
    ///
    /// Separate from the unsigned pass because the interesting magnitudes are the ones that leave a
    /// remainder — that is where converting the signed value directly would floor a wei further out
    /// than taking `abs()` first.
    function test_writeSignedSyUtilsFixtures() public {
        uint256[5] memory ix = indices();
        int256[6] memory am = [
            int256(1),
            int256(1_000_000),
            int256(1e18),
            int256(999_999_999_999_999_999),
            int256(83_658_000_000_000_000_000),
            int256(0)
        ];

        for (uint256 i = 0; i < ix.length; i++) {
            PYIndex index = PYIndex.wrap(ix[i]);
            for (uint256 k = 0; k < am.length; k++) {
                signedRows(ix[i], index, am[k]);
                if (am[k] != 0) {
                    signedRows(ix[i], index, -am[k]);
                }
            }
        }

        string memory body = "";
        for (uint256 i = 0; i < rows.length; i++) {
            body = string.concat(body, rows[i], i + 1 == rows.length ? "" : ",");
        }
        vm.writeFile(
            "../tests/fixtures/sy_utils_signed.json", string.concat('{"cases":[', body, "]}")
        );
    }

    function signedRows(uint256 raw, PYIndex index, int256 amount) internal {
        row("sy_to_asset_i", raw, vm.toString(amount), vm.toString(index.syToAsset(amount)));
        row("asset_to_sy_i", raw, vm.toString(amount), vm.toString(index.assetToSy(amount)));
        row("asset_to_sy_up_i", raw, vm.toString(amount), vm.toString(index.assetToSyUp(amount)));
    }

    /// A zero index divides by zero in the asset→SY direction and is merely zero in the other.
    /// Recorded so the port fails on the same half rather than on both or neither.
    function test_writeZeroIndexFixtures() public {
        string[] memory cases = new string[](4);
        uint256 n = 0;

        (bool ok,) = address(this).call(abi.encodeCall(this.assetToSy, (0, 1)));
        require(!ok, "assetToSy at a zero index should revert");
        cases[n++] = '{"op":"asset_to_sy","outcome":"revert"}';

        (ok,) = address(this).call(abi.encodeCall(this.assetToSyUp, (0, 1)));
        require(!ok, "assetToSyUp at a zero index should revert");
        cases[n++] = '{"op":"asset_to_sy_up","outcome":"revert"}';

        cases[n++] = string.concat(
            '{"op":"sy_to_asset","outcome":"ok","y":"',
            vm.toString(SYUtils.syToAsset(0, 1e18)),
            '"}'
        );
        cases[n++] = string.concat(
            '{"op":"sy_to_asset_up","outcome":"ok","y":"',
            vm.toString(SYUtils.syToAssetUp(0, 1e18)),
            '"}'
        );

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, cases[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile(
            "../tests/fixtures/sy_utils_zero_index.json",
            string.concat('{"cases":[', body, "]}")
        );
    }

    function assetToSy(uint256 index, uint256 amount) external pure returns (uint256) {
        return SYUtils.assetToSy(index, amount);
    }

    function assetToSyUp(uint256 index, uint256 amount) external pure returns (uint256) {
        return SYUtils.assetToSyUp(index, amount);
    }

    function row(string memory op, uint256 index, string memory amount, string memory y) internal {
        rows.push(
            string.concat(
                '{"op":"',
                op,
                '","index":"',
                vm.toString(index),
                '","amount":"',
                amount,
                '","y":"',
                y,
                '"}'
            )
        );
    }
}
