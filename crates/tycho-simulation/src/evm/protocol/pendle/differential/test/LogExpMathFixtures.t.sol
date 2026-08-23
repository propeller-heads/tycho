// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {LogExpMath} from "pendle/core/libraries/math/LogExpMath.sol";

/// Writes the fixtures the Rust port is asserted against, by evaluating the Solidity original.
///
/// This contract does not reimplement anything — it calls the upstream library and records what it
/// returns. The Rust side then has to agree bit for bit. A port that is merely close is a port that
/// disagrees with the contract on some input, and a quote that disagrees with the contract is a
/// wrong quote.
///
/// Run `./regenerate.sh` rather than invoking this directly; it clones the upstream sources first.
contract LogExpMathFixtures is Test {
    string constant OUT = "../tests/fixtures/log_exp_math.json";

    int256 constant ONE_18 = 1e18;
    int256 constant MAX_NATURAL_EXPONENT = 130e18;
    int256 constant MIN_NATURAL_EXPONENT = -41e18;

    /// The powers of two `exp` decomposes its argument into. Each is a branch boundary, and each is
    /// probed exactly, one below and one above.
    function expDecompositionPoints() internal pure returns (int256[12] memory p) {
        p[0] = 128e18; // x0, 2^7
        p[1] = 64e18; // x1, 2^6
        p[2] = 32e18; // x2, 2^5
        p[3] = 16e18; // x3, 2^4
        p[4] = 8e18; // x4, 2^3
        p[5] = 4e18; // x5, 2^2
        p[6] = 2e18; // x6, 2^1
        p[7] = 1e18; // x7, 2^0
        p[8] = 5e17; // x8, 2^-1
        p[9] = 25e16; // x9, 2^-2
        p[10] = 125e15; // x10, 2^-3
        p[11] = 625e14; // x11, 2^-4
    }

    function test_writeExpFixtures() public {
        int256[] memory xs = expInputs();
        string[] memory rows = new string[](xs.length);
        for (uint256 i = 0; i < xs.length; i++) {
            rows[i] = string.concat(
                '{"x":"', vm.toString(xs[i]), '","y":"', vm.toString(LogExpMath.exp(xs[i])), '"}'
            );
        }
        writeArray("exp", rows);
    }

    function test_writeLnFixtures() public {
        int256[] memory as_ = lnInputs();
        string[] memory rows = new string[](as_.length);
        for (uint256 i = 0; i < as_.length; i++) {
            rows[i] = string.concat(
                '{"a":"', vm.toString(as_[i]), '","y":"', vm.toString(LogExpMath.ln(as_[i])), '"}'
            );
        }
        writeArray("ln", rows);
    }

    /// Both domain guards, recorded as reverts so the Rust port is pinned to error where the
    /// contract errors rather than to return a plausible number.
    function test_expRevertsOutsideItsDomain() public {
        vm.expectRevert();
        this.expWrapper(MAX_NATURAL_EXPONENT + 1);
        vm.expectRevert();
        this.expWrapper(MIN_NATURAL_EXPONENT - 1);
    }

    function test_lnRevertsAtOrBelowZero() public {
        vm.expectRevert();
        this.lnWrapper(0);
        vm.expectRevert();
        this.lnWrapper(-1);
    }

    function expWrapper(int256 x) external pure returns (int256) {
        return LogExpMath.exp(x);
    }

    function lnWrapper(int256 a) external pure returns (int256) {
        return LogExpMath.ln(a);
    }

    function expInputs() internal pure returns (int256[] memory xs) {
        int256[12] memory p = expDecompositionPoints();
        // Each decomposition point exactly, and either side of it: the branches are `>=`, so the
        // boundary is where an off-by-one port diverges.
        int256[] memory buf = new int256[](3 * 12 + 3 * 12 + 14);
        uint256 n = 0;
        for (uint256 i = 0; i < 12; i++) {
            buf[n++] = p[i] - 1;
            buf[n++] = p[i];
            buf[n++] = p[i] + 1;
            buf[n++] = -p[i] - 1;
            buf[n++] = -p[i];
            buf[n++] = -p[i] + 1;
        }
        buf[n++] = MIN_NATURAL_EXPONENT;
        buf[n++] = MIN_NATURAL_EXPONENT + 1;
        buf[n++] = MAX_NATURAL_EXPONENT;
        buf[n++] = MAX_NATURAL_EXPONENT - 1;
        buf[n++] = 0;
        buf[n++] = 1;
        buf[n++] = -1;
        // Sums of several decomposition points, so more than one branch is taken at once.
        buf[n++] = 129e18;
        buf[n++] = 127e18 + 5e17;
        buf[n++] = 100e18;
        buf[n++] = 63e18;
        buf[n++] = 3e18 + 14e16;
        // Rate-space values of the size Pendle actually evaluates: ln(1.0005)-ish fee roots and
        // implied rates.
        buf[n++] = 499875041000000;
        buf[n++] = 20211000000000000;
        // The negative mirrors of the larger decomposition points fall outside the domain — `exp`
        // only accepts [-41e18, 130e18] — so they are dropped here. The domain edges themselves are
        // covered by the values added above, and the reverts by their own test.
        uint256 kept = 0;
        for (uint256 i = 0; i < n; i++) {
            if (buf[i] >= MIN_NATURAL_EXPONENT && buf[i] <= MAX_NATURAL_EXPONENT) {
                buf[kept++] = buf[i];
            }
        }
        xs = new int256[](kept);
        for (uint256 i = 0; i < kept; i++) {
            xs[i] = buf[i];
        }
    }

    function lnInputs() internal pure returns (int256[] memory as_) {
        int256[] memory buf = new int256[](40);
        uint256 n = 0;
        // The ln_36 window and its edges: inside it a different, higher-precision path runs, and
        // the bounds are strict inequalities.
        buf[n++] = ONE_18 - 1e17; // LN_36_LOWER_BOUND, excluded
        buf[n++] = ONE_18 - 1e17 + 1; // first value inside
        buf[n++] = ONE_18 + 1e17; // LN_36_UPPER_BOUND, excluded
        buf[n++] = ONE_18 + 1e17 - 1; // last value inside
        buf[n++] = ONE_18; // ln(1) = 0
        buf[n++] = ONE_18 - 1;
        buf[n++] = ONE_18 + 1;
        // Below one, which recurses through the reciprocal.
        buf[n++] = 1;
        buf[n++] = 1e6;
        buf[n++] = 1e12;
        buf[n++] = 5e17;
        buf[n++] = 89e16;
        // Above one, across the a_n decomposition.
        buf[n++] = 111e16;
        buf[n++] = 2e18;
        buf[n++] = 3e18;
        buf[n++] = 10e18;
        buf[n++] = 1000e18;
        buf[n++] = 1e36;
        buf[n++] = 1e40;
        // Exchange rates and PY indices of the shape Pendle carries.
        buf[n++] = 1241884000000000000;
        buf[n++] = 1095830;
        buf[n++] = 1000000000000000000;
        as_ = new int256[](n);
        for (uint256 i = 0; i < n; i++) {
            as_[i] = buf[i];
        }
    }

    function writeArray(string memory key, string[] memory rows) internal {
        string memory body = "";
        for (uint256 i = 0; i < rows.length; i++) {
            body = string.concat(body, rows[i], i + 1 == rows.length ? "" : ",");
        }
        string memory json = string.concat('{"', key, '":[', body, "]}");
        string memory path = string.concat("../tests/fixtures/", key, ".json");
        vm.writeFile(path, json);
    }
}
