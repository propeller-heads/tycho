// SPDX-License-Identifier: MIT
pragma solidity 0.8.35;

/// @title MaxOfTwoPool
/// @notice A test pool for FlammSampler and the ladder rule. One runtime, with no constructor and no immutables, serves
///         an Efficient pool's `previewSwap` and `pairRequest` and its pair hook's `quotePair`, `levState` and
///         `params()`, so one address can be both. Two concave integer routes, each `r * a / (x + a)`: the curve,
///         labelled with the curve fee, and the leverage route, labelled with the row's spread on a sale and its
///         spreadDn on a buy. The better one fills, a tie stays on the curve, and a refused leverage route falls to the
///         curve. A test writes each route and switch into its slot before it calls; zero turns a feature off.
/// @dev Token 0 is the pool asset: a sale sells it (direction 0), a buy buys it (direction 1). Slots:
///      0 token 0; 1 token 1; 2 the fault bits below; 3 the taker, for the caller check; 4 the gas a burner leaves;
///      5 a bomb's size in bytes; 6 a revert reason, in the low 4 bytes; 7 the live band, request word 10;
///      8, 9 token 0's and token 1's USD prices, WAD; 10 p36, token 1 units per token 0 unit scaled by 1e36, for the
///      band lines; 11 the curve fee, `quotePair` word 3; 12 `levState` word 6, the bid; 13 `levState` word 0, cv;
///      16-19 token 0's legs and 20-23 token 1's: held, supplied, posted, owed.
///      Routes, at 0x40 + 0x10 * direction + k:
///      0, 1 the curve's r and x; 2, 3 the leverage route's r and x, r zero for none;
///      4 dropped edge: leverage refuses above this size;
///      5, 6 hole rows: leverage refuses where `a % slot5 == slot6`;
///      7 far-edge lottery: leverage refuses where its output is at or under the band's far line, or above it by at
///        most a jitter in [0, slot7), the far line at the lower of the live band and the row's (params word 32);
///      8, 9 floor flicker: leverage refuses on [slot8, slot8 + slot9);
///      10, 11 a fall: leverage pays slot11 less above size slot10, the join on a buy and the net's fall on a sale;
///      12 token ceiling: leverage refuses a net over this, which is also request word 14;
///      13 a clip: the curve uses at most this input, and the preview re-asks at the input it used.
///      `params()` word k is slot 0x100 + k: 0-2 A, spanUp, spanDn; 27 spread, 28 spreadDn, 29 upCap, 30 dnCap,
///      31 ceiling, 32 band, 33 feed gate, 34 upFloor. Products assume sizes and routes under 2^120 and p36 under 2^135.
contract MaxOfTwoPool {
    uint256 internal constant CALLER = 1 << 0; // previewSwap and pairRequest revert NotTaker() unless called by slot 3
    uint256 internal constant BURN = 1 << 1; // previewSwap burns gas down to slot 4, then answers; zero: it runs out
    uint256 internal constant RETURN_BOMB = 1 << 2; // previewSwap answers slot 5 bytes
    uint256 internal constant REVERT_BOMB = 1 << 3; // previewSwap reverts with slot 5 bytes
    uint256 internal constant SHORT = 1 << 4; // previewSwap answers 64 bytes, used and out
    uint256 internal constant REASON = 1 << 5; // previewSwap reverts with slot 6's reason: ReadStarved(), a tag
    uint256 internal constant BARE = 1 << 6; // previewSwap reverts with no data
    uint256 internal constant EQUAL_FEE = 1 << 7; // the leverage route carries the curve fee
    uint256 internal constant CORE_BAND = 1 << 8; // a fill under the live band's far line reverts: the dust windows
    uint256 internal constant USED_1BP = 1 << 9; // used is the ask less ask / 10,000, within 1 bp of it
    uint256 internal constant REQUEST_REVERT = 1 << 10; // pairRequest reverts
    uint256 internal constant LEV_REVERT = 1 << 11; // levState reverts
    uint256 internal constant LEV_SHORT = 1 << 12; // levState answers 7 words

    uint256 internal constant WAD = 1e18;
    uint256 internal constant E36 = 1e36;

    error InvalidAmount();
    error NotServed();
    error NotTaker();
    error Refused();
    error PriceBand();
    error RequestRefused();
    error LevStateRefused();

    fallback() external {
        bytes4 s = msg.sig;
        if (s == 0xbbbaffab) _preview(); // previewSwap(address,address,uint256)
        if (s == 0x6a198209) _request(); // pairRequest(address,address,uint256)
        if (s == 0x2f4de491) _quotePair(); // quotePair(PairRequest)
        if (s == 0xd38f9bd9) _levState(); // levState(PairRequest)
        if (s == 0xcff0ab96) _params(); // params()
        revert NotServed();
    }

    function _preview() private view {
        (uint256 dir, uint256 a, uint256 f) = _args();
        if (f & BURN != 0) {
            uint256 keep = _s(4);
            assembly {
                for {} gt(gas(), keep) {} {}
            }
        }
        if (f & (RETURN_BOMB | REVERT_BOMB) != 0) {
            uint256 n = _s(5);
            if (f & RETURN_BOMB != 0) assembly { return(0, n) }
            assembly { revert(0, n) }
        }
        if (f & REASON != 0) _fail(uint32(_s(6)));
        if (f & BARE != 0) assembly { revert(0, 0) }
        unchecked {
            uint256 u = f & USED_1BP != 0 ? a - a / 10_000 : a;
            (uint256 used, uint256 out, uint256 fee) = _best(dir, u, f);
            if (used < u) (used, out, fee) = _best(dir, used, f); // the re-ask at the input used
            if (out == 0) revert Refused();
            if (f & CORE_BAND != 0 && _under(dir, used, out, _s(7))) revert PriceBand();
            uint256 size = f & SHORT != 0 ? 64 : 96;
            assembly {
                mstore(0, used)
                mstore(32, out)
                mstore(64, fee)
                return(0, size)
            }
        }
    }

    /// @dev The better route at input `a`: (used, out, label), out zero where both refuse.
    function _best(uint256 dir, uint256 a, uint256 f) private view returns (uint256, uint256, uint256) {
        unchecked {
            uint256 base = 0x40 + 0x10 * dir;
            uint256 cu = a;
            uint256 clip = _s(base + 13);
            if (clip != 0 && cu > clip) cu = clip;
            uint256 c = _route(_s(base), _s(base + 1), cu);
            (bool ok, uint256 l) = _lev(dir, base, a);
            if (ok && l > c) return (a, l, f & EQUAL_FEE != 0 ? _s(11) : _s(0x100 + 27 + dir));
            return (cu, c, _s(11));
        }
    }

    /// @dev The leverage route at `a`, which it uses whole or refuses.
    function _lev(uint256 dir, uint256 base, uint256 a) private view returns (bool, uint256 out) {
        unchecked {
            uint256 r = _s(base + 2);
            uint256 edge = _s(base + 4);
            uint256 m = _s(base + 5);
            uint256 lo = _s(base + 8);
            uint256 len = _s(base + 9);
            if (r == 0 || (edge != 0 && a > edge) || (m != 0 && a % m == _s(base + 6))) return (false, 0);
            if (len != 0 && a >= lo && a - lo < len) return (false, 0);
            out = _route(r, _s(base + 3), a);
            uint256 by = _s(base + 11);
            if (by != 0 && a > _s(base + 10)) out = out > by ? out - by : 0;
            uint256 cap = _s(base + 12);
            if (out == 0 || (cap != 0 && out > cap)) return (false, 0);
            uint256 j = _s(base + 7);
            if (j != 0) {
                uint256 b = _s(0x100 + 32);
                uint256 live = _s(7);
                uint256 line = _line(dir, a, live < b ? live : b);
                if (out <= line || out - line <= uint256(keccak256(abi.encode(a))) % j) return (false, 0);
            }
            return (true, out);
        }
    }

    function _route(uint256 r, uint256 x, uint256 a) private pure returns (uint256) {
        unchecked {
            return r * a / (x + a);
        }
    }

    /// @dev The band's far line at input `a`: the least output a fill of `a` may pay at band `b`.
    function _line(uint256 dir, uint256 a, uint256 b) private view returns (uint256) {
        unchecked {
            uint256 p36 = _s(10);
            if (b >= WAD || p36 == 0) return 0;
            return dir == 0 ? a * (p36 * (WAD - b) / WAD) / E36 : a * (E36 * (WAD - b) / WAD) / p36;
        }
    }

    /// @dev Whether `out` for `used` sits under the live band's far line, as core's check on a whole-unit output does.
    function _under(uint256 dir, uint256 used, uint256 out, uint256 b) private view returns (bool) {
        unchecked {
            uint256 p36 = _s(10);
            if (b >= WAD || p36 == 0) return false;
            return dir == 0 ? out * E36 < used * (p36 * (WAD - b) / WAD) : out * p36 < used * (E36 * (WAD - b) / WAD);
        }
    }

    function _request() private view {
        (uint256 dir, uint256 a, uint256 f) = _args();
        if (f & REQUEST_REVERT != 0) revert RequestRefused();
        uint256[26] memory w;
        unchecked {
            w[0] = uint160(msg.sender);
            w[1] = _s(dir);
            w[2] = _s(1 - dir);
            w[3] = dir;
            w[4] = 1 - dir;
            w[10] = _s(7);
            w[11] = _s(8 + dir);
            w[12] = _s(9 - dir);
            w[13] = a;
            uint256 cap = _s(0x40 + 0x10 * dir + 12);
            w[14] = cap == 0 ? type(uint256).max : cap;
            for (uint256 k; k < 4; ++k) {
                w[15 + k] = _s(16 + 4 * dir + k);
                w[19 + k] = _s(20 - 4 * dir + k);
            }
        }
        assembly {
            return(w, 832)
        }
    }

    /// @dev The curve's fee in word 3 of a 7-word fill, whatever the request.
    function _quotePair() private view {
        uint256 fee = _s(11);
        assembly {
            mstore(0, 0)
            mstore(32, 0)
            mstore(64, 0)
            mstore(96, fee)
            mstore(128, 0)
            mstore(160, 0)
            mstore(192, 0)
            return(0, 224)
        }
    }

    /// @dev Eight words, cv in word 0 and the bid in word 6, whatever the request.
    function _levState() private view {
        uint256 f = _s(2);
        if (f & LEV_REVERT != 0) revert LevStateRefused();
        uint256[8] memory v;
        v[0] = _s(13);
        v[6] = _s(12);
        uint256 size = f & LEV_SHORT != 0 ? 224 : 256;
        assembly {
            return(v, size)
        }
    }

    function _params() private view {
        uint256[35] memory v;
        for (uint256 k; k < 35; ++k) {
            v[k] = _s(0x100 + k);
        }
        assembly {
            return(v, 1120)
        }
    }

    /// @dev The direction, the input and the fault bits, after the pair, amount and caller checks.
    function _args() private view returns (uint256 dir, uint256 a, uint256 f) {
        address tokenIn;
        address tokenOut;
        assembly {
            tokenIn := and(calldataload(4), 0xffffffffffffffffffffffffffffffffffffffff)
            tokenOut := and(calldataload(36), 0xffffffffffffffffffffffffffffffffffffffff)
            a := calldataload(68)
        }
        address t0 = address(uint160(_s(0)));
        address t1 = address(uint160(_s(1)));
        if (tokenIn == t1 && tokenOut == t0) dir = 1;
        else if (tokenIn != t0 || tokenOut != t1) revert NotServed();
        if (a == 0) revert InvalidAmount();
        f = _s(2);
        if (f & CALLER != 0 && msg.sender != address(uint160(_s(3)))) revert NotTaker();
    }

    function _fail(uint256 reason) private pure {
        assembly {
            mstore(0, shl(224, reason))
            revert(0, 4)
        }
    }

    function _s(uint256 slot) private view returns (uint256 v) {
        assembly {
            v := sload(slot)
        }
    }
}
