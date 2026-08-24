// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {PMath} from "pendle/core/libraries/math/PMath.sol";

/// Writes the fixtures the Rust port of `PMath` is asserted against.
///
/// The rounding *direction* is the whole reason this library exists: Pendle picks `Down` or `Up`
/// per call site so that every remainder lands with the protocol. A port that rounds the other way
/// is off by one in the user's favour, and the contract then refuses to settle. So the grid is
/// built out of operands that do **not** divide evenly — an exact-division grid would pass against
/// either rounding direction and prove nothing.
///
/// `market.json` and `approx.json` already exercise most of this library indirectly, but only along
/// the paths those two happen to take. These fixtures pin each helper on its own.
///
/// Run `./regenerate.sh` rather than invoking this directly.
contract PMathFixtures is Test {
    /// Accumulated in storage rather than a local array: the grid is built by a dozen helpers and
    /// threading a memory array plus a counter through all of them exhausts the stack.
    string[] internal rows;

    /// The router's own constants, so the acceptance test is probed where it actually runs.
    uint256 constant EPS = 50_000_000_000_000; // 5e13, a hundredth of a percent
    uint256 constant GUESS_RANGE_TWEAK = 50_000_000_000_000_000; // 5e16, the +/-5% window
    uint256 constant ONE = 1e18;

    function test_writePMathFixtures() public {
        mulDownCases();
        divDownCases();
        signedCases();
        boundCases();
        approxCases();
        castCases();

        string memory body = "";
        for (uint256 i = 0; i < rows.length; i++) {
            body = string.concat(body, rows[i], i + 1 == rows.length ? "" : ",");
        }
        vm.writeFile("../tests/fixtures/pmath.json", string.concat('{"cases":[', body, "]}"));
    }

    function mulDownCases() internal {
        // A product that does not divide evenly by ONE: the remainder must be lost, not rounded.
        uint256 third = ONE / 3;
        pushU("mul_down_u", 3 * ONE, third, 0);
        // The same operands as raw integers rather than fixed-point, which truncates to nothing.
        // The units mistake this library exists to prevent, recorded so the port reproduces it.
        pushU("mul_down_u", 3, third, 0);
        pushU("mul_down_u", ONE, ONE, 0);
        pushU("mul_down_u", 0, ONE, 0);
        pushU("mul_down_u", ONE, 0, 0);
        pushU("mul_down_u", 1, 1, 0);
        pushU("mul_down_u", ONE - 1, ONE - 1, 0);
        // Values of the size the market actually carries.
        pushU("mul_down_u", 86_364_560_000_000_000_000, 1_241_884_000_000_000_000, 0);
        pushU("mul_down_u", 1_429_719_000_000_000_000_000, 1_095_830, 0);
        pushU("mul_down_u", 83_658_000_000_000_000_000, 960_000_000_000_000_000, 0);

        pushU("raw_div_up", 10, 3, 0);
        pushU("raw_div_up", 9, 3, 0);
        pushU("raw_div_up", 0, 3, 0);
        pushU("raw_div_up", 1, 1, 0);
        pushU("raw_div_up", ONE - 1, ONE, 0);
        pushU("raw_div_up", 7, 2, 0);

        pushU("sub_max_0", 5, 3, 0);
        pushU("sub_max_0", 3, 5, 0);
        pushU("sub_max_0", 0, 0, 0);
        pushU("sub_max_0", ONE, 1, 0);
    }

    function divDownCases() internal {
        pushU("div_down_u", 1, 3, 0);
        pushU("div_down_u", 2, 3, 0);
        pushU("div_down_u", ONE, 3 * ONE, 0);
        pushU("div_down_u", ONE, ONE, 0);
        pushU("div_down_u", 0, 7, 0);
        pushU("div_down_u", 1, ONE, 0);
        // Across the decimal gap, in both directions.
        pushU("div_down_u", 1_095_830, ONE, 0);
        pushU("div_down_u", ONE, 1_095_830, 0);
    }

    function signedCases() internal {
        int256 third = int256(ONE) / 3;
        // Signed division truncates toward zero in both languages, so the negative case must keep
        // the same magnitude as the positive one rather than flooring a wei further out.
        pushI("mul_down_i", -3 * int256(ONE), third, 0);
        pushI("mul_down_i", 3 * int256(ONE), third, 0);
        pushI("mul_down_i", -1, 1, 0);
        pushI("mul_down_i", -1, int256(ONE), 0);
        pushI("mul_down_i", 1, -int256(ONE), 0);
        pushI("mul_down_i", -int256(ONE), -int256(ONE), 0);
        pushI("mul_down_i", 0, -5 * int256(ONE), 0);

        pushI("div_down_i", 1, 3, 0);
        pushI("div_down_i", -1, 3, 0);
        pushI("div_down_i", 1, -3, 0);
        pushI("div_down_i", -1, -3, 0);
        pushI("div_down_i", int256(ONE), 3 * int256(ONE), 0);
        pushI("div_down_i", -int256(ONE), 3 * int256(ONE), 0);

        pushI("sub_no_neg", 5, 3, 0);
        pushI("sub_no_neg", 3, 3, 0);
        pushI("sub_no_neg", 0, 0, 0);
        // Both negative, which still satisfies `a >= b`.
        pushI("sub_no_neg", -3, -5, 0);
        pushI("sub_no_neg", -5, -5, 0);
    }

    function boundCases() internal {
        // `tweakUp`/`tweakDown` build the search's initial window, so the factor probed is the one
        // the router uses.
        pushU("tweak_up", ONE, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_down", ONE, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_up", 83_658_000_000_000_000_000, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_down", 83_658_000_000_000_000_000, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_up", 1_000_000, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_down", 1_000_000, GUESS_RANGE_TWEAK, 0);
        // A value small enough that a 5% haircut truncates it away entirely.
        pushU("tweak_down", 1, GUESS_RANGE_TWEAK, 0);
        pushU("tweak_up", ONE, 0, 0);
        pushU("tweak_down", ONE, 0, 0);

        pushU("clamp", 5, 1, 10);
        pushU("clamp", 0, 1, 10);
        pushU("clamp", 20, 1, 10);
        // Both bounds exactly, where an off-by-one in the comparison would show.
        pushU("clamp", 1, 1, 10);
        pushU("clamp", 10, 1, 10);

        pushU("add_with_upper_bound", 5, 3, 100);
        pushU("add_with_upper_bound", 5, 3, 7);
        pushU("add_with_upper_bound", 5, 3, 8);
        // The overflow branch: the sum cannot be represented, so the bound is returned rather than
        // wrapping. This is the branch the search relies on when it walks its range outward.
        pushU("add_with_upper_bound", type(uint256).max - 1, 5, 100);
        pushU("add_with_upper_bound", 0, 0, 0);

        pushU("sub_with_lower_bound", 5, 3, 0);
        pushU("sub_with_lower_bound", 3, 5, 0);
        pushU("sub_with_lower_bound", 5, 3, 4);
        pushU("sub_with_lower_bound", 0, 1, 7);
        pushU("sub_with_lower_bound", 5, 5, 0);
    }

    function approxCases() internal {
        // The loop accepts a guess only from below, within eps. `ONE - EPS` scaled by `b` is the
        // exact acceptance floor, so it is probed at the floor and one wei either side.
        uint256 b = ONE;
        uint256 floor_ = PMath.mulDown(b, ONE - EPS);
        pushU3("is_a_smaller_approx_b", b, b, EPS);
        pushU3("is_a_smaller_approx_b", b - 1, b, EPS);
        pushU3("is_a_smaller_approx_b", b + 1, b, EPS);
        pushU3("is_a_smaller_approx_b", floor_, b, EPS);
        pushU3("is_a_smaller_approx_b", floor_ - 1, b, EPS);
        pushU3("is_a_smaller_approx_b", floor_ + 1, b, EPS);
        pushU3("is_a_smaller_approx_b", 0, b, EPS);
        // A non-round `b`, so the acceptance floor itself carries a truncation.
        uint256 odd = 83_658_000_000_000_000_001;
        pushU3("is_a_smaller_approx_b", PMath.mulDown(odd, ONE - EPS), odd, EPS);
        pushU3("is_a_smaller_approx_b", PMath.mulDown(odd, ONE - EPS) - 1, odd, EPS);
    }

    function castCases() internal {
        pushU("to_i256", 7, 0, 0);
        pushU("to_i256", 0, 0, 0);
        pushU("to_i256", uint256(type(int256).max), 0, 0);
        pushI("to_u256", 7, 0, 0);
        pushI("to_u256", 0, 0, 0);
        pushI("to_u256", type(int256).max, 0, 0);
    }

    /// The guards that revert. Recorded as a separate file so the Rust port is pinned to error
    /// where the contract errors, rather than to return a plausible number.
    function test_writePMathRevertFixtures() public {
        string[] memory reverts = new string[](8);
        uint256 n = 0;

        reverts[n++] = revertRow("sub_no_neg_goes_negative", abi.encodeCall(this.subNoNeg, (3, 4)));
        reverts[n++] = revertRow(
            "to_i256_above_int256_max",
            abi.encodeCall(this.toI256, (uint256(type(int256).max) + 1))
        );
        reverts[n++] = revertRow("to_u256_of_negative", abi.encodeCall(this.toU256, (-1)));
        reverts[n++] = revertRow("div_down_u_by_zero", abi.encodeCall(this.divDownU, (1, 0)));
        reverts[n++] = revertRow("div_down_i_by_zero", abi.encodeCall(this.divDownI, (1, 0)));
        reverts[n++] = revertRow("raw_div_up_by_zero", abi.encodeCall(this.rawDivUp, (1, 0)));
        // `mulDown` multiplies before dividing, so a product past 256 bits reverts even though the
        // quotient would have fit.
        reverts[n++] = revertRow(
            "mul_down_u_overflows",
            abi.encodeCall(this.mulDownU, (type(uint256).max, 2))
        );

        string memory body = "";
        for (uint256 i = 0; i < n; i++) {
            body = string.concat(body, reverts[i], i + 1 == n ? "" : ",");
        }
        vm.writeFile(
            "../tests/fixtures/pmath_reverts.json", string.concat('{"reverts":[', body, "]}")
        );
    }

    function revertRow(string memory label, bytes memory call) internal returns (string memory) {
        (bool ok,) = address(this).call(call);
        require(!ok, string.concat("expected a revert for ", label));
        return string.concat('{"case":"', label, '"}');
    }

    // `external` wrappers so the reverts above can be caught rather than aborting the run.
    function subNoNeg(int256 a, int256 b) external pure returns (int256) {
        return PMath.subNoNeg(a, b);
    }

    function toI256(uint256 x) external pure returns (int256) {
        return PMath.Int(x);
    }

    function toU256(int256 x) external pure returns (uint256) {
        return PMath.Uint(x);
    }

    function divDownU(uint256 a, uint256 b) external pure returns (uint256) {
        return PMath.divDown(a, b);
    }

    function divDownI(int256 a, int256 b) external pure returns (int256) {
        return PMath.divDown(a, b);
    }

    function rawDivUp(uint256 a, uint256 b) external pure returns (uint256) {
        return PMath.rawDivUp(a, b);
    }

    function mulDownU(uint256 a, uint256 b) external pure returns (uint256) {
        return PMath.mulDown(a, b);
    }

    /// Evaluates one unsigned two-argument op and records it. `c` is unused by these and written as
    /// zero, so every row has the same shape.
    function pushU(string memory op, uint256 a, uint256 b, uint256 c) internal {
        uint256 y;
        if (eq(op, "mul_down_u")) y = PMath.mulDown(a, b);
        else if (eq(op, "div_down_u")) y = PMath.divDown(a, b);
        else if (eq(op, "raw_div_up")) y = PMath.rawDivUp(a, b);
        else if (eq(op, "sub_max_0")) y = PMath.subMax0(a, b);
        else if (eq(op, "tweak_up")) y = PMath.tweakUp(a, b);
        else if (eq(op, "tweak_down")) y = PMath.tweakDown(a, b);
        else if (eq(op, "clamp")) y = PMath.clamp(a, b, c);
        else if (eq(op, "add_with_upper_bound")) y = PMath.addWithUpperBound(a, b, c);
        else if (eq(op, "sub_with_lower_bound")) y = PMath.subWithLowerBound(a, b, c);
        else if (eq(op, "to_i256")) y = uint256(PMath.Int(a));
        else revert(string.concat("unknown unsigned op ", op));
        row(op, vm.toString(a), vm.toString(b), vm.toString(c), vm.toString(y));
    }

    /// The three-argument acceptance test, whose result is a bool recorded as 1 or 0.
    function pushU3(string memory op, uint256 a, uint256 b, uint256 c) internal {
        require(eq(op, "is_a_smaller_approx_b"), "unknown predicate");
        bool y = PMath.isASmallerApproxB(a, b, c);
        row(op, vm.toString(a), vm.toString(b), vm.toString(c), y ? "1" : "0");
    }

    function pushI(string memory op, int256 a, int256 b, int256 c) internal {
        int256 y;
        if (eq(op, "mul_down_i")) y = PMath.mulDown(a, b);
        else if (eq(op, "div_down_i")) y = PMath.divDown(a, b);
        else if (eq(op, "sub_no_neg")) y = PMath.subNoNeg(a, b);
        else if (eq(op, "to_u256")) y = int256(PMath.Uint(a));
        else revert(string.concat("unknown signed op ", op));
        row(op, vm.toString(a), vm.toString(b), vm.toString(c), vm.toString(y));
    }

    function row(
        string memory op,
        string memory a,
        string memory b,
        string memory c,
        string memory y
    ) internal {
        rows.push(
            string.concat(
                '{"op":"', op, '","a":"', a, '","b":"', b, '","c":"', c, '","y":"', y, '"}'
            )
        );
    }

    function eq(string memory a, string memory b) internal pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
}
