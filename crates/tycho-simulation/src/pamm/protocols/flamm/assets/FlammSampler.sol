// SPDX-License-Identifier: MIT
pragma solidity 0.8.35;

/// @title FlammSampler
/// @notice Never deployed. A node runs this by eth_call state override at the address whose calls a pool should see.
///         In one call it walks an Efficient pool's `previewSwap` in each direction asked, bisects where the fee label
///         changes, reads the request words the ladder rule needs, and returns every probe. It writes nothing and never
///         reverts, so a revert at that address means the sampler did not run.
/// @dev The steps, breadth first across walks, so a short gas budget leaves every direction coarse rather than one
///      complete:
///      1. seeds, ascending, round robin;
///      2. once per walk with a tier label, the request's band, USD prices and legs, and where they decide anything
///         the curve's fee and the lever-up's top bid;
///      3. bisection of each label change;
///      4. bisection towards the first refusal;
///      5. splits of the same-label spans whose chord may lose most, only with a non-zero `gapPpm`;
///      6. one probe above a tier-labelled top.
///      Before each call to the pool or hook the gas gate requires `gasleft() >= probeGas * 64 / 63 + reserveGas`, so
///      the child really gets its cap; where it fails, the walk stops and reads `truncated`. Only priced probes paying
///      at least `DUST_OUT` count towards the fall, third-label, request, flip, edge and probe-above logic, since the
///      pool's band check, made on a whole-unit output, can refuse between small prices.
contract FlammSampler {
    bytes4 public constant TAG = 0xed7b4ac9; // bytes4(keccak256("FlammSampler"))

    uint256 internal constant DUST_OUT = 10_000; // under this output a refusal ends nothing: the band check rounds a whole-unit output
    uint256 internal constant ABOVE_SHIFT = 10; // a tier-labelled top is probed once more at used + (used >> ABOVE_SHIFT) + 1

    uint256 internal constant PRICED = 1; // a whole fill, or a clip within a rounding
    uint256 internal constant CLIPPED = 2; // used < ask by more than 1 part in 10,000
    uint256 internal constant REFUSED = 3; // a revert
    uint256 internal constant STARVED = 4; // says nothing about the pool
    uint256 internal constant MALFORMED = 5; // an answer that cannot be a fill

    bytes4 internal constant READ_STARVED = 0x67521dd6; // ReadStarved(): a venue read that ran short of gas
    bytes4 internal constant GAS_SHORT = 0x882f2cb8; // GasShort()

    uint256 internal constant MAX_POINT = 1 << 120; // asks and answers stay under this
    uint256 internal constant NONE = type(uint256).max; // no probe

    struct Ask {
        address tokenIn;
        address tokenOut;
        address hook; // the pair's hook; zero: neither the request nor the curve fee is read
        uint256 tierA; // the row's spreadWad; zero: no row
        uint256 tierB; // the row's spreadDnWad
        uint256 maxProbes; // this walk's budget; a request read takes one from what is left, and the probe above may add one past it
        uint256[] seeds; // ascending, above zero, under 2^120
    }

    struct Limits {
        uint256 probeGas; // the cap on each call to the pool or hook
        uint256 reserveGas; // gas kept to encode the answer
        uint256 flipShift; // bisect a label change while hi - lo > lo >> flipShift
        uint256 edgeProbes; // bisection steps towards the first refusal
        uint256 gapPpm; // split a same-label span whose chord may lose more; zero: off
    }

    struct Probe {
        uint256 ask;
        uint256 used;
        uint256 out;
        uint256 feeWad;
        uint256 gasUsed;
        uint256 status;
        bytes4 reason; // a revert's first four bytes; zero on a bare revert
    }

    struct Walk {
        Probe[] probes; // ascending by ask, at most maxProbes + 1
        uint256 curveFeeWad; // zero: not read
        uint64 bandWad; // word 10 of the request; zero: not read
        uint256 usdIn; // word 11
        uint256 usdOut; // word 12
        uint256[8] legs; // words 15-22: heldIn, suppliedIn, postedIn, owedIn, heldOut, suppliedOut, postedOut, owedOut
        uint256 bidWad; // word 6 of hook.levState, read on a buy; zero: not read, or no top bid
        bool truncated; // the gas gate stopped this walk early
    }

    /// @dev One walk's progress, kept beside its `Walk` and not returned.
    struct Run {
        address tokenIn;
        address tokenOut;
        address hook;
        uint256 tierA;
        uint256 tierB;
        uint256 n; // probes made
        uint256 cap; // maxProbes + 1, the length of the walk's probe array
        uint256 budget; // probes left before the probe above
        uint256 next; // the next seed
        uint256 lastOut; // the last seed priced at or above DUST_OUT, and its label
        uint256 lastFee;
        uint256 labelA; // the distinct labels of those seeds, up to two
        uint256 labelB;
        uint256 labels;
        uint256 edgeSteps;
        bool seeding;
        bool priced; // a seed has priced at or above DUST_OUT
        bool read; // the request read has run
        bool stopped; // the gas gate stopped the walk
        bool edgesDone;
        bool refineDone;
    }

    /// @notice Walks `pool` once per ask, every call made from this code's own address.
    /// @param pool The pool whose `previewSwap` and `pairRequest` are called.
    /// @param asks One walk each: a direction, the pair's hook and its tier labels, a probe budget and the seeds.
    /// @param limits The per-call cap, the gas kept back, and the bisection and refine settings.
    /// @return tag `TAG`, so a caller can tell this code ran.
    /// @return blockNumber The block the walks read.
    /// @return walks Each ask's probes, ascending by ask, and the request words read.
    function sample(address pool, Ask[] calldata asks, Limits calldata limits)
        external
        view
        returns (bytes4 tag, uint256 blockNumber, Walk[] memory walks)
    {
        unchecked {
            tag = TAG;
            blockNumber = block.number;
            uint256 count = asks.length;
            walks = new Walk[](count);
            Run[] memory runs = new Run[](count);
            uint256 widest;
            for (uint256 i; i < count; ++i) {
                Ask calldata a = asks[i];
                Run memory r = runs[i];
                (r.tokenIn, r.tokenOut, r.hook, r.tierA, r.tierB) = (a.tokenIn, a.tokenOut, a.hook, a.tierA, a.tierB);
                r.cap = a.maxProbes + 1; // room for the probe above
                r.budget = a.maxProbes;
                r.seeding = true;
                walks[i].probes = new Probe[](r.cap);
                if (r.cap > widest) widest = r.cap;
            }
            // the request's call data, reused by every read: a selector, then the 26 words pairRequest answers
            uint256 buf;
            assembly ("memory-safe") {
                buf := mload(0x40)
                mstore(0x40, add(buf, 864))
            }
            _seeds(pool, asks, limits, walks, runs);
            for (uint256 i; i < count; ++i) {
                _tryRead(pool, limits, walks[i], runs[i], buf);
            }
            _flips(pool, limits, walks, runs, buf);
            _edges(pool, limits, walks, runs, buf);
            if (limits.gapPpm != 0) _refine(pool, limits, walks, runs, buf, widest);
            for (uint256 i; i < count; ++i) {
                _above(pool, limits, walks[i], runs[i], buf);
            }
            for (uint256 i; i < count; ++i) {
                Probe[] memory ps = walks[i].probes;
                uint256 n = runs[i].n;
                assembly ("memory-safe") {
                    mstore(ps, n)
                }
            }
        }
    }

    // ------------------------------------------------------------------ 1. seeds

    /// @dev Round robin across walks, ascending. A walk stops seeding at its budget, at STARVED, MALFORMED or
    ///      CLIPPED, and once it has a price paying at least DUST_OUT, at a refusal, at a fall with an unchanged label
    ///      or at a third label. Before that price a refusal is dust and the walk goes on.
    function _seeds(address pool, Ask[] calldata asks, Limits calldata l, Walk[] memory walks, Run[] memory runs)
        private
        view
    {
        unchecked {
            uint256 count = asks.length;
            for (bool any = true; any;) {
                any = false;
                for (uint256 i; i < count; ++i) {
                    Run memory r = runs[i];
                    if (!r.seeding) continue;
                    uint256[] calldata seeds = asks[i].seeds;
                    if (r.stopped || r.budget == 0 || r.next >= seeds.length) {
                        r.seeding = false;
                        continue;
                    }
                    (uint256 k,) = _probe(pool, l, walks[i], r, seeds[r.next]);
                    ++r.next;
                    any = true;
                    if (k == NONE) r.seeding = false;
                    else _judge(r, walks[i].probes[k]);
                }
            }
        }
    }

    /// @dev Whether a seed's answer ends the walk's seeding; it records the last price and the labels seen.
    function _judge(Run memory r, Probe memory p) private pure {
        unchecked {
            uint256 s = p.status;
            if (s == STARVED || s == MALFORMED || s == CLIPPED) {
                r.seeding = false;
                return;
            }
            if (s == REFUSED) {
                if (r.priced) r.seeding = false;
                return;
            }
            if (p.out < DUST_OUT) return;
            uint256 f = p.feeWad;
            if (r.priced && p.out < r.lastOut && f == r.lastFee) {
                r.seeding = false;
                return;
            }
            if (r.labels == 0) {
                (r.labelA, r.labels) = (f, 1);
            } else if (f != r.labelA && (r.labels == 1 || f != r.labelB)) {
                if (r.labels == 1) {
                    (r.labelB, r.labels) = (f, 2);
                } else {
                    r.labels = 3;
                    r.seeding = false;
                    return;
                }
            }
            (r.priced, r.lastOut, r.lastFee) = (true, p.out, f);
        }
    }

    // ------------------------------------------------------------------ 2. the request

    /// @dev Reads the request once, at the lowest ask of a probe priced or clipped at or above DUST_OUT with a tier
    ///      label, on a walk with a hook and a row. Asked after the seeds and after each later probe until it runs.
    function _tryRead(address pool, Limits calldata l, Walk memory w, Run memory r, uint256 buf) private view {
        unchecked {
            if (r.read || r.stopped || r.hook == address(0) || r.tierA == 0) return;
            Probe[] memory ps = w.probes;
            uint256 n = r.n;
            for (uint256 k; k < n; ++k) {
                Probe memory p = ps[k];
                if (_counts(p) && _tier(r, p.feeWad)) {
                    _read(pool, l, w, r, buf, p.ask);
                    return;
                }
            }
        }
    }

    /// @dev `pool.pairRequest` at `ask`: words 10-12 and 15-22. Then, where the probes counted so far carry one label
    ///      and it is a tier, `hook.quotePair` on those 26 words with word 13 zeroed, keeping word 3 of the fill, the
    ///      curve's fee; and on a buy (word 3 of the request is 1) `hook.levState` on the 26 words as answered,
    ///      keeping word 6, the lever-up's top bid. Its calls together take one probe from the budget, and run even on
    ///      none. Each copies only what it keeps, and any failure leaves its words zero.
    function _read(address pool, Limits calldata l, Walk memory w, Run memory r, uint256 buf, uint256 ask)
        private
        view
    {
        unchecked {
            r.read = true;
            if (r.budget != 0) --r.budget;
            if (!_open(l)) {
                _stop(w, r);
                return;
            }
            uint256 cap = l.probeGas;
            (address tokenIn, address tokenOut, address hook) = (r.tokenIn, r.tokenOut, r.hook);
            bool ok;
            assembly ("memory-safe") {
                mstore(buf, shl(224, 0x6a198209)) // pairRequest(address,address,uint256)
                mstore(add(buf, 4), tokenIn)
                mstore(add(buf, 36), tokenOut)
                mstore(add(buf, 68), ask)
                let called := staticcall(cap, pool, buf, 100, 0, 0)
                ok := and(called, eq(returndatasize(), 832))
                if ok { returndatacopy(add(buf, 4), 0, 832) }
            }
            if (!ok) return;
            uint256 band = _word(buf, 10);
            if (band > type(uint64).max) return; // a request's band is a uint64: this answer is not one
            w.bandWad = uint64(band);
            w.usdIn = _word(buf, 11);
            w.usdOut = _word(buf, 12);
            uint256[8] memory legs = w.legs;
            for (uint256 j; j < 8; ++j) {
                legs[j] = _word(buf, 15 + j);
            }
            if (_oneTierLabel(w, r)) {
                if (!_open(l)) {
                    _stop(w, r);
                    return;
                }
                uint256 fee;
                assembly ("memory-safe") {
                    let word13 := add(buf, 420) // word 13, amountIn
                    let amount := mload(word13)
                    mstore(word13, 0)
                    // quotePair(PairRequest): only the selector changes, and word 13 reads zero for this call
                    mstore(buf, or(shl(224, 0x2f4de491), and(mload(buf), sub(shl(224, 1), 1))))
                    let called := staticcall(cap, hook, buf, 836, 0, 0)
                    if and(called, eq(returndatasize(), 224)) {
                        returndatacopy(0, 96, 32)
                        fee := mload(0)
                    }
                    mstore(word13, amount)
                }
                w.curveFeeWad = fee;
            }
            if (_word(buf, 3) == 1) {
                if (!_open(l)) {
                    _stop(w, r);
                    return;
                }
                uint256 bid;
                assembly ("memory-safe") {
                    // levState(PairRequest): only the selector changes, over the 26 words as answered
                    mstore(buf, or(shl(224, 0xd38f9bd9), and(mload(buf), sub(shl(224, 1), 1))))
                    let called := staticcall(cap, hook, buf, 836, 0, 0)
                    if and(called, eq(returndatasize(), 256)) {
                        returndatacopy(0, 192, 32)
                        bid := mload(0)
                    }
                }
                w.bidWad = bid;
            }
        }
    }

    /// @dev Word `k` of the request held at `buf`, past its selector.
    function _word(uint256 buf, uint256 k) private pure returns (uint256 v) {
        assembly ("memory-safe") {
            v := mload(add(add(buf, 4), shl(5, k)))
        }
    }

    /// @dev Whether the probes counted so far carry exactly one label, and it is a tier.
    function _oneTierLabel(Walk memory w, Run memory r) private pure returns (bool) {
        unchecked {
            Probe[] memory ps = w.probes;
            uint256 n = r.n;
            uint256 label;
            bool seen;
            for (uint256 k; k < n; ++k) {
                Probe memory p = ps[k];
                if (!_counts(p)) continue;
                if (!seen) (label, seen) = (p.feeWad, true);
                else if (p.feeWad != label) return false;
            }
            return seen && _tier(r, label);
        }
    }

    // ------------------------------------------------------------------ 3. flips

    /// @dev While a walk has budget, bisects its lowest pair of adjacent probes, both priced at or above DUST_OUT,
    ///      whose labels differ and whose asks are at least two and more than `lo >> flipShift` apart. Alternates
    ///      between walks.
    function _flips(address pool, Limits calldata l, Walk[] memory walks, Run[] memory runs, uint256 buf)
        private
        view
    {
        unchecked {
            uint256 shift = l.flipShift;
            uint256 count = walks.length;
            for (bool any = true; any;) {
                any = false;
                for (uint256 i; i < count; ++i) {
                    Run memory r = runs[i];
                    if (r.stopped || r.budget == 0) continue;
                    Walk memory w = walks[i];
                    Probe[] memory ps = w.probes;
                    uint256 n = r.n;
                    for (uint256 k = 1; k < n; ++k) {
                        Probe memory lo = ps[k - 1];
                        Probe memory hi = ps[k];
                        if (!_good(lo) || !_good(hi) || lo.feeWad == hi.feeWad) continue;
                        uint256 width = hi.ask - lo.ask;
                        if (width < 2 || width <= lo.ask >> shift) continue;
                        (, bool made) = _probe(pool, l, w, r, lo.ask + width / 2);
                        if (made) {
                            _tryRead(pool, l, w, r, buf);
                            any = true;
                        }
                        break;
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ 4. edges

    /// @dev Up to `edgeProbes` bisection steps between the last priced probe paying at least DUST_OUT and the first
    ///      refusal above it, while they are at least two apart and budget remains.
    function _edges(address pool, Limits calldata l, Walk[] memory walks, Run[] memory runs, uint256 buf)
        private
        view
    {
        unchecked {
            uint256 steps = l.edgeProbes;
            uint256 count = walks.length;
            for (bool any = true; any;) {
                any = false;
                for (uint256 i; i < count; ++i) {
                    Run memory r = runs[i];
                    if (r.edgesDone || r.stopped) continue;
                    if (r.edgeSteps >= steps || r.budget == 0) {
                        r.edgesDone = true;
                        continue;
                    }
                    Walk memory w = walks[i];
                    Probe[] memory ps = w.probes;
                    uint256 n = r.n;
                    uint256 lo = NONE;
                    uint256 hi = NONE;
                    for (uint256 k; k < n; ++k) {
                        Probe memory p = ps[k];
                        if (_good(p)) {
                            lo = k;
                        } else if (p.status == REFUSED && lo != NONE) {
                            hi = k;
                            break;
                        }
                    }
                    if (hi == NONE || ps[hi].ask - ps[lo].ask < 2) {
                        r.edgesDone = true;
                        continue;
                    }
                    uint256 a = ps[lo].ask;
                    ++r.edgeSteps;
                    (, bool made) = _probe(pool, l, w, r, a + (ps[hi].ask - a) / 2);
                    if (made) {
                        _tryRead(pool, l, w, r, buf);
                        any = true;
                    } else {
                        // the midpoint was probed already, so no later step makes a probe; or the walk stopped
                        r.edgesDone = true;
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ 5. refine

    /// @dev While budget remains, splits the same-label span over PRICED probes whose chord may lose most against
    ///      its neighbours' slopes, relative to `gapPpm` of its lower output.
    function _refine(
        address pool,
        Limits calldata l,
        Walk[] memory walks,
        Run[] memory runs,
        uint256 buf,
        uint256 widest
    ) private view {
        unchecked {
            uint256 ppm = l.gapPpm;
            uint256 count = walks.length;
            uint256[] memory idx = new uint256[](widest);
            for (bool any = true; any;) {
                any = false;
                for (uint256 i; i < count; ++i) {
                    Run memory r = runs[i];
                    if (r.refineDone || r.stopped) continue;
                    if (r.budget == 0) {
                        r.refineDone = true;
                        continue;
                    }
                    Walk memory w = walks[i];
                    (bool found, uint256 ask) = _split(w, r, ppm, idx);
                    if (!found) {
                        r.refineDone = true;
                        continue;
                    }
                    (, bool made) = _probe(pool, l, w, r, ask);
                    if (!made) {
                        r.refineDone = true;
                        continue;
                    }
                    _tryRead(pool, l, w, r, buf);
                    any = true;
                }
            }
        }
    }

    /// @dev The span to split, if any: the midpoint of its asks. Widths are on `used` and `out`, over PRICED probes
    ///      only. For a same-label span of widths `dx`, `dy` (`dy` floored at zero), `A` scales the previous
    ///      same-label span's rise to `dx` (the span's own `dy` where there is none), `B` the next one's (`2 dy - A`,
    ///      floored at zero, where there is none), and a span with `g = (A - B) / 16` over `t = max(1, lo.out * ppm /
    ///      1e6)` is a candidate; the largest `g / t` wins, the first on a tie. A neighbour with `dx == 0` counts as
    ///      absent, a span with `dx == 0` is skipped, and so is one whose midpoint is a probe of any status. Points
    ///      are under 2^120, so `A`, `B` and `g` stay under 2^240 and the ratios compare in 512 bits. Where
    ///      `lo.out * ppm` reaches 2^256, `t` would pass 2^236, which no `g` reaches, so that span is no candidate.
    function _split(Walk memory w, Run memory r, uint256 ppm, uint256[] memory idx)
        private
        pure
        returns (bool found, uint256 ask)
    {
        unchecked {
            Probe[] memory ps = w.probes;
            uint256 n = r.n;
            uint256 m;
            for (uint256 k; k < n; ++k) {
                if (ps[k].status == PRICED) idx[m++] = k;
            }
            uint256 bestG;
            uint256 bestT = 1;
            for (uint256 j = 1; j < m; ++j) {
                Probe memory lo = ps[idx[j - 1]];
                Probe memory hi = ps[idx[j]];
                if (lo.feeWad != hi.feeWad || hi.used <= lo.used) continue;
                uint256 width = hi.ask - lo.ask;
                if (width < 2) continue;
                uint256 mid = lo.ask + width / 2;
                bool taken;
                for (uint256 q = idx[j - 1] + 1; q < idx[j]; ++q) {
                    if (ps[q].ask == mid) {
                        taken = true;
                        break;
                    }
                }
                if (taken) continue;
                uint256 dx = hi.used - lo.used;
                uint256 dy = hi.out > lo.out ? hi.out - lo.out : 0;
                uint256 a = dy;
                if (j >= 2) {
                    Probe memory pl = ps[idx[j - 2]];
                    if (pl.feeWad == lo.feeWad && lo.used > pl.used) {
                        a = (lo.out > pl.out ? lo.out - pl.out : 0) * dx / (lo.used - pl.used);
                    }
                }
                uint256 b;
                bool right;
                if (j + 1 < m) {
                    Probe memory nh = ps[idx[j + 1]];
                    if (nh.feeWad == lo.feeWad && nh.used > hi.used) {
                        b = (nh.out > hi.out ? nh.out - hi.out : 0) * dx / (nh.used - hi.used);
                        right = true;
                    }
                }
                if (!right) b = 2 * dy > a ? 2 * dy - a : 0;
                if (a <= b) continue;
                uint256 g = (a - b) / 16;
                (uint256 over, uint256 t) = _mul512(lo.out, ppm);
                if (over != 0) continue;
                t /= 1e6;
                if (t == 0) t = 1;
                if (g > t && (!found || _gt512(g, bestT, bestG, t))) (found, ask, bestG, bestT) = (true, mid, g, t);
            }
        }
    }

    /// @dev Whether `a * b > c * d`, in 512 bits.
    function _gt512(uint256 a, uint256 b, uint256 c, uint256 d) private pure returns (bool) {
        (uint256 h1, uint256 l1) = _mul512(a, b);
        (uint256 h2, uint256 l2) = _mul512(c, d);
        return h1 > h2 || (h1 == h2 && l1 > l2);
    }

    function _mul512(uint256 a, uint256 b) private pure returns (uint256 hi, uint256 lo) {
        assembly ("memory-safe") {
            let mm := mulmod(a, b, not(0))
            lo := mul(a, b)
            hi := sub(sub(mm, lo), lt(mm, lo))
        }
    }

    // ------------------------------------------------------------------ 6. the probe above

    /// @dev On a walk with a hook and a row, where its PRICED probe of highest ask paying at least DUST_OUT carries a
    ///      tier label, probes `used + (used >> ABOVE_SHIFT) + 1` once, on whatever budget remains, even none. An ask
    ///      at or above 2^120 is not made, and one already probed adds nothing.
    function _above(address pool, Limits calldata l, Walk memory w, Run memory r, uint256 buf) private view {
        unchecked {
            if (r.stopped || r.hook == address(0) || r.tierA == 0) return;
            Probe[] memory ps = w.probes;
            uint256 n = r.n;
            uint256 top = NONE;
            for (uint256 k; k < n; ++k) {
                if (_good(ps[k])) top = k;
            }
            if (top == NONE || !_tier(r, ps[top].feeWad)) return;
            uint256 used = ps[top].used;
            uint256 a = used + (used >> ABOVE_SHIFT) + 1;
            if (a >= MAX_POINT) return;
            (, bool made) = _probe(pool, l, w, r, a);
            if (made) _tryRead(pool, l, w, r, buf);
        }
    }

    // ------------------------------------------------------------------ one probe

    /// @dev The probe at `ask`: its index and whether it was made now. An ask already probed returns its probe and
    ///      is not made again. None is made once the walk has stopped or its array is full, or where the gas gate
    ///      fails, which stops the walk. A probe made is put in order and takes one from the budget, if any is left.
    function _probe(address pool, Limits calldata l, Walk memory w, Run memory r, uint256 ask)
        private
        view
        returns (uint256 k, bool made)
    {
        unchecked {
            Probe[] memory ps = w.probes;
            uint256 n = r.n;
            for (; k < n; ++k) {
                uint256 x = ps[k].ask;
                if (x == ask) return (k, false);
                if (x > ask) break;
            }
            if (r.stopped || n >= r.cap) return (NONE, false);
            if (!_open(l)) {
                _stop(w, r);
                return (NONE, false);
            }
            Probe memory p = ps[n]; // the next unused element, allocated up front
            _call(pool, r.tokenIn, r.tokenOut, ask, l.probeGas, p);
            for (uint256 j = n; j > k; --j) {
                ps[j] = ps[j - 1];
            }
            ps[k] = p;
            r.n = n + 1;
            if (r.budget != 0) --r.budget;
            made = true;
        }
    }

    /// @dev One `previewSwap` of `ask` at `cap`, classified in this order: over 7/8 of the cap spent, STARVED whatever
    ///      came back; a revert with `ReadStarved` or `GasShort`, STARVED; any other revert, bare included, REFUSED;
    ///      an answer that is not 96 bytes, or `used` zero, past `ask` or at or above 2^120, or `out` zero or at or
    ///      above 2^120, MALFORMED; `used` short of `ask` by more than 1 part in 10,000, CLIPPED; otherwise PRICED. It
    ///      copies at most 96 bytes of an answer and 4 of a revert, so a return bomb costs nothing here, and a burner
    ///      costs its cap and the call's own access cost.
    function _call(address pool, address tokenIn, address tokenOut, uint256 ask, uint256 cap, Probe memory p)
        private
        view
    {
        unchecked {
            uint256 ok;
            uint256 size;
            uint256 spent;
            uint256 used;
            uint256 out;
            uint256 fee;
            bytes4 reason;
            assembly ("memory-safe") {
                let m := mload(0x40)
                mstore(m, shl(224, 0xbbbaffab)) // previewSwap(address,address,uint256)
                mstore(add(m, 4), tokenIn)
                mstore(add(m, 36), tokenOut)
                mstore(add(m, 68), ask)
                let before := gas()
                ok := staticcall(cap, pool, m, 100, 0, 0)
                spent := sub(before, gas())
                size := returndatasize()
                switch ok
                case 0 {
                    if gt(size, 3) {
                        returndatacopy(m, 0, 4)
                        reason := and(mload(m), shl(224, 0xffffffff))
                    }
                }
                default {
                    if eq(size, 96) {
                        returndatacopy(m, 0, 96)
                        used := mload(m)
                        out := mload(add(m, 32))
                        fee := mload(add(m, 64))
                    }
                }
            }
            uint256 status;
            if (spent * 8 > cap * 7) {
                status = STARVED;
            } else if (ok == 0) {
                status = reason == READ_STARVED || reason == GAS_SHORT ? STARVED : REFUSED;
            } else if (
                size != 96 || used == 0 || out == 0 || used > ask || used >= MAX_POINT || out >= MAX_POINT
            ) {
                status = MALFORMED;
            } else if (used * 10_000 < ask * 9_999) {
                status = CLIPPED;
            } else {
                status = PRICED;
            }
            (p.ask, p.used, p.out, p.feeWad, p.gasUsed, p.status, p.reason) =
                (ask, used, out, fee, spent, status, reason);
        }
    }

    // ------------------------------------------------------------------ helpers

    /// @dev The gas gate: the child of the next call gets its whole cap, and `reserveGas` stays for the answer.
    function _open(Limits calldata l) private view returns (bool) {
        unchecked {
            return gasleft() >= l.probeGas * 64 / 63 + l.reserveGas;
        }
    }

    function _stop(Walk memory w, Run memory r) private pure {
        w.truncated = true;
        r.stopped = true;
    }

    /// @dev Priced or clipped, paying at least DUST_OUT: what the request read counts.
    function _counts(Probe memory p) private pure returns (bool) {
        return (p.status == PRICED || p.status == CLIPPED) && p.out >= DUST_OUT;
    }

    /// @dev PRICED, paying at least DUST_OUT: what the flips, edges and the probe above count.
    function _good(Probe memory p) private pure returns (bool) {
        return p.status == PRICED && p.out >= DUST_OUT;
    }

    function _tier(Run memory r, uint256 fee) private pure returns (bool) {
        return r.tierA != 0 && (fee == r.tierA || fee == r.tierB);
    }
}
