pragma solidity ^0.8.26;

import "./EkuboV3MevCaptureV21.t.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {tickToSqrtRatio} from "@ekubo-v3/math/ticks.sol";
import {amount0Delta, amount1Delta} from "@ekubo-v3/math/delta.sol";

string constant MEV_CAPTURE_V21_GAS_ROWS =
    "../../tycho-simulation/src/evm/protocol/ekubo_v3/assets/mev_capture_v21_gas.json";

contract MevCaptureV21GasTestToken is ERC20 {
    constructor() ERC20("Gas test token", "GAS") {}
}

/// Gas of swaps through the executor on MEVCapture v2.1 pools and on twin
/// pools without extension (same tokens, fee, tick spacing and positions),
/// which the test creates on an Ethereum fork against the deployed extension
/// and Core. Every call runs as its own transaction (`isolate`), so each swap
/// starts with cold accounts and storage and with no slot written yet, as a
/// mainnet swap does.
///
/// The tycho-simulation gas model of these pools is fitted to and tested
/// against the swaps recorded in `MEV_CAPTURE_V21_GAS_ROWS`
/// (`ekubo_v3::pool::mev_capture_v21::tests::test_gas_matches_fork_measurements`):
/// both pools' states before each swap, both gas figures and both outputs.
/// This test checks that the swaps still execute to the recorded outputs and
/// cost the recorded difference between the pools, within 1%. Setting
/// `MEV_CAPTURE_V21_GAS_WRITE=true` rewrites the file instead; the Rust test
/// then shows whether the model still fits.
contract EkuboV3MevCaptureV21GasTest is Constants, TestUtils {
    using CoreLib for ICore;

    uint64 constant FEE_30BP = 55340232221128654;
    uint64 constant FEE_5BP = 9223372036854775;
    int128 constant L = int128(1) << 80;
    int32 constant WIDE = 40_000;
    /// Receives the outputs. It already holds the tokens, as most swappers do.
    address constant SINK = address(0x5157);

    struct Position {
        int32 lower;
        int32 upper;
        int128 liquidity;
    }

    struct Pools {
        string name;
        PoolKey plain;
        PoolKey v21;
        /// Liquidity `_rowTo` sizes swaps with.
        uint128 liquidity;
    }

    struct Recorder {
        bool write;
        string recorded;
        string pools;
        string rows;
        uint256 count;
    }

    EkuboV3ExecutorStandalone immutable executor =
        new EkuboV3ExecutorStandalone(EKUBO_V3_SIGNED_EXCLUSIVE_SWAP);
    LiquidityHelper immutable liquidityHelper = new LiquidityHelper();

    constructor() {
        vm.makePersistent(address(executor));
        vm.makePersistent(address(liquidityHelper));
    }

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), MEV_CAPTURE_V21_FORK_BLOCK);

        _fund(USDC_ADDR);
        _fund(USDT_ADDR);
    }

    function _fund(address token) internal {
        deal(token, address(liquidityHelper), 1e30);
        deal(token, SINK, 1);
    }

    /// forge-config: default.isolate = true
    function testSwapsMatchRecordedGas() public {
        Recorder memory rec;
        rec.write = vm.envOr("MEV_CAPTURE_V21_GAS_WRITE", false);
        if (!rec.write) rec.recorded = vm.readFile(MEV_CAPTURE_V21_GAS_ROWS);

        _dense(rec);
        _empty(rec);
        _step(rec);
        _far(rec);
        _bitmapWords(rec);
        _refresh(rec);
        _route(rec);

        if (rec.write) {
            vm.writeFile(
                MEV_CAPTURE_V21_GAS_ROWS,
                string.concat(
                    "{\"pools\":{",
                    rec.pools,
                    "},\n\"rows\":[\n",
                    rec.rows,
                    "\n]}\n"
                )
            );
        } else {
            assertFalse(
                vm.keyExistsJson(
                    rec.recorded,
                    string.concat(".rows[", vm.toString(rec.count), "]")
                ),
                "more rows recorded than measured"
            );
        }
    }

    /// Initialized ticks on every spacing from 2 to 65 spacings either side
    /// of the price, traded through before.
    function _dense(Recorder memory rec) internal {
        Pools memory p = _pools(
            rec,
            "dense",
            FEE_30BP,
            100,
            0,
            _narrow(_wide(0), 100, -6500, 6500, 0)
        );
        _warmUp(p, 7000);
        _rowTo(rec, p, "dense_away_inc", 150, 12);
        _rowTo(rec, p, "dense_away_inc_bridges_same_ts", 450, 0);
        _rowTo(rec, p, "dense_toward_dec_ticks", 250, 12);
        _rowTo(rec, p, "dense_toward_away_dec", -650, 12);
        _rowTo(rec, p, "dense_away_dec_same_ts", -1050, 0);
        _rowTo(rec, p, "dense_toward_away_inc", 1500, 12);
        _rowTo(rec, p, "dense_away_inc_long", 3500, 12);
        _rowTo(rec, p, "dense_toward_dec_long", 0, 12);
        _rowTo(rec, p, "dense_away_dec_long", -2600, 12);
        _rowTo(rec, p, "dense_toward_inc_same_ts", -1900, 0);
    }

    /// No initialized tick near the price.
    function _empty(Recorder memory rec) internal {
        Pools memory p = _pools(rec, "empty", FEE_5BP, 100, 0, _wide(0));
        _rowTo(rec, p, "empty_away_inc_1", 100, 12);
        _rowTo(rec, p, "empty_away_inc_4", 400, 12);
        _rowTo(rec, p, "empty_away_inc_16_same_ts", 1600, 0);
        _rowTo(rec, p, "empty_away_inc_long", 5000, 12);
        _rowTo(rec, p, "empty_toward_dec", 2000, 12);
        _rowTo(rec, p, "empty_toward_away_dec", -3000, 12);
        _rowTo(rec, p, "empty_away_dec_same_ts", -6000, 0);
        _rowTo(rec, p, "empty_toward_inc", -100, 12);
        _rowTo(rec, p, "empty_away_inc_8", 750, 12);
        _rowTo(rec, p, "empty_away_inc_same_ts", 950, 0);
    }

    /// 8 times the liquidity above tick 2000.
    function _step(Recorder memory rec) internal {
        Position[] memory positions = new Position[](2);
        positions[0] = Position(-WIDE, WIDE, L);
        positions[1] = Position(2000, WIDE, 7 * L);
        Pools memory p = _pools(rec, "step", FEE_30BP, 100, 0, positions);
        _warmUp(p, 3000);
        _rowTo(rec, p, "step_away_inc", 500, 12);
        _rowTo(rec, p, "step_into_step_same_ts", 2500, 0);
        _rowTo(rec, p, "step_away_inc_in_step", 3000, 12);
        _rowTo(rec, p, "step_toward_dec_out_of_step", 1500, 132);
        _rowTo(rec, p, "step_into_step_again_same_ts", 2600, 0);
    }

    /// Far from tick 0, with initialized ticks around the price.
    function _far(Recorder memory rec) internal {
        int32 c = -200_000;
        Pools memory p = _pools(
            rec,
            "far",
            FEE_5BP,
            100,
            c,
            _narrow(_wide(c), 100, c - 3300, c + 3300, c)
        );
        _warmUp(p, 3500);
        _rowTo(rec, p, "far_away_dec", c - 450, 12);
        _rowTo(rec, p, "far_toward_away_inc", c + 1250, 12);
        _rowTo(rec, p, "far_away_inc_same_ts", c + 2900, 0);
        _rowTo(rec, p, "far_toward_dec", c + 600, 12);
        _rowTo(rec, p, "far_toward_away_dec_long", c - 3000, 12);
    }

    /// Swaps across empty tick bitmap words, which end Core calls of the
    /// extension away from the anchor and add loop steps toward it.
    function _bitmapWords(Recorder memory rec) internal {
        Pools memory p = _pools(rec, "words", FEE_30BP, 10, 0, _wide(0));
        _rowTo(rec, p, "words_away_inc", 3000, 12);
        _rowTo(rec, p, "words_toward_away_dec", -2000, 12);
        _rowTo(rec, p, "words_toward_away_inc", 8000, 12);
        _rowTo(rec, p, "words_toward_away_dec_long", -9000, 12);
        _rowTo(rec, p, "words_away_dec_same_ts", -9100, 0);

        p = _pools(rec, "unit", FEE_5BP, 1, 0, _wide(0));
        _rowTo(rec, p, "unit_away_inc", 300, 12);
        _rowTo(rec, p, "unit_away_inc_same_ts", 1100, 0);
        _rowTo(rec, p, "unit_toward_dec", 600, 12);
        _rowTo(rec, p, "unit_toward_away_dec", -900, 12);

        p = _pools(rec, "spacing100", FEE_5BP, 100, 0, _wide(0));
        _rowTo(rec, p, "spacing100_away_inc", 14_000, 12);
        _rowTo(rec, p, "spacing100_toward_dec", 11_000, 12);
        _rowTo(rec, p, "spacing100_toward_away_dec", -13_500, 12);
    }

    /// Liquidity of 41 bits, which the extension's reference reaches in 41
    /// half-lives, and 8 times as much above tick 2000. A later swap of a
    /// timestamp that ends there raises the reference again in `_refresh`,
    /// which writes the extension's pool state.
    function _refresh(Recorder memory rec) internal {
        int128 small = int128(1) << 40;
        Position[] memory positions = new Position[](2);
        positions[0] = Position(-WIDE, WIDE, small);
        positions[1] = Position(2000, WIDE, 7 * small);
        Pools memory p = _pools(rec, "refresh", FEE_30BP, 100, 0, positions);
        // The reference rises by one bit per half-life (120 s), up to the 41
        // bits of the liquidity.
        for (uint256 i; i < 42; i++) {
            _nextBlock(120);
            _swapBoth(p, i % 2 == 0 ? int32(150) : int32(-150));
        }
        _rowTo(rec, p, "refresh_first_of_ts", 300, 120);
        _rowTo(rec, p, "refresh_into_range_same_ts", 2400, 0);
        _rowTo(rec, p, "refresh_in_range", 2600, 12);
    }

    /// The pool and swaps of `EkuboV3MevCaptureV21Test`, on pools traded
    /// through their initialized ticks first.
    function _route(Recorder memory rec) internal {
        PoolKey memory v21 = mevCaptureV21PoolKey();
        Pools memory p = Pools({
            name: "route",
            plain: PoolKey({
                token0: v21.token0,
                token1: v21.token1,
                config: createConcentratedPoolConfig(
                    v21.config.fee(),
                    v21.config.concentratedTickSpacing(),
                    address(0)
                )
            }),
            v21: v21,
            liquidity: 40_000_000_000_000
        });
        int128 l = 40_000_000_000_000;
        Position[] memory positions = new Position[](5);
        positions[0] = Position(-50_000, 50_000, l);
        positions[1] = Position(-3000, -1000, l);
        positions[2] = Position(1000, 3000, l);
        positions[3] = Position(2000, 4000, l / 2);
        positions[4] = Position(4000, 6000, l / 2);
        _create(rec, p, 0, positions);
        _warmUp(p, 15_000);

        _row(rec, p, "route_away_dec", false, 50_000e6, 12);
        _row(rec, p, "route_toward_inc", true, 10_000e6, 0);
        _row(rec, p, "route_away_inc", true, 210_000e6, 12);
        _row(rec, p, "route_toward_dec", false, 40_000e6, 0);
    }

    function _wide(int32 center) internal pure returns (Position[] memory p) {
        p = new Position[](1);
        p[0] = Position(center - WIDE, center + WIDE, L);
    }

    /// `base` and a narrow position on every other spacing `s` from 2 spacings
    /// beyond `center` up to `upper` and down to `lower`.
    function _narrow(
        Position[] memory base,
        int32 s,
        int32 lower,
        int32 upper,
        int32 center
    ) internal pure returns (Position[] memory p) {
        uint256 n = base.length;
        uint256 up = uint256(int256((upper - center - s) / (2 * s)));
        uint256 down = uint256(int256((center - s - lower) / (2 * s)));
        p = new Position[](n + up + down);
        for (uint256 i; i < n; i++) {
            p[i] = base[i];
        }
        for (uint256 i; i < up; i++) {
            int32 t = center + 2 * s * int32(int256(i + 1));
            p[n + i] = Position(t, t + s, L / 512);
        }
        for (uint256 i; i < down; i++) {
            int32 t = center - 2 * s * int32(int256(i + 1));
            p[n + up + i] = Position(t - s, t, L / 512);
        }
    }

    function _pools(
        Recorder memory rec,
        string memory name,
        uint64 fee,
        uint32 spacing,
        int32 tick,
        Position[] memory positions
    ) internal returns (Pools memory p) {
        // Fresh tokens, so that each layout gets its own pools.
        address token0 = address(new MevCaptureV21GasTestToken());
        address token1 = address(new MevCaptureV21GasTestToken());
        if (token0 > token1) (token0, token1) = (token1, token0);
        _fund(token0);
        _fund(token1);
        p.name = name;
        p.plain = PoolKey({
            token0: token0,
            token1: token1,
            config: createConcentratedPoolConfig(fee, spacing, address(0))
        });
        p.v21 = PoolKey({
            token0: token0,
            token1: token1,
            config: createConcentratedPoolConfig(
                fee, spacing, MEV_CAPTURE_V21_ADDRESS
            )
        });
        p.liquidity = uint128(positions[0].liquidity);
        _create(rec, p, tick, positions);
    }

    function _create(
        Recorder memory rec,
        Pools memory p,
        int32 tick,
        Position[] memory positions
    ) internal {
        CORE.initializePool(p.plain, tick);
        CORE.initializePool(p.v21, tick);
        string memory json = "";
        for (uint256 i; i < positions.length; i++) {
            Position memory q = positions[i];
            liquidityHelper.provide(p.plain, q.lower, q.upper, q.liquidity);
            liquidityHelper.provide(p.v21, q.lower, q.upper, q.liquidity);
            json = string.concat(
                json,
                i == 0 ? "[" : ",[",
                vm.toString(q.lower),
                ",",
                vm.toString(q.upper),
                ",\"",
                vm.toString(q.liquidity),
                "\"]"
            );
        }
        rec.pools = string.concat(
            rec.pools,
            bytes(rec.pools).length == 0 ? "\n\"" : ",\n\"",
            p.name,
            "\":{\"fee\":\"",
            vm.toString(uint256(p.v21.config.fee())),
            "\",\"tickSpacing\":",
            vm.toString(uint256(p.v21.config.concentratedTickSpacing())),
            ",\"positions\":[",
            json,
            "]}"
        );
    }

    /// Trades both pools across all initialized ticks within `reach` of the
    /// price and back, so the measured swaps update tick fee snapshots as on a
    /// traded pool. In the pools' first timestamp, so the anchor stays.
    function _warmUp(Pools memory p, int32 reach) internal {
        int32 start = CORE.poolState(toPoolId(p.plain)).tick();
        _swapBoth(p, start + reach);
        _swapBoth(p, start - reach);
        _swapBoth(p, start);
    }

    /// The input that moves the plain pool's price to `tick` through
    /// `p.liquidity` alone, before fees.
    function _amountTo(Pools memory p, int32 tick)
        internal
        view
        returns (bool isToken1, uint256 amount)
    {
        SqrtRatio current = CORE.poolState(toPoolId(p.plain)).sqrtRatio();
        SqrtRatio target = tickToSqrtRatio(tick);
        isToken1 = SqrtRatio.unwrap(target) > SqrtRatio.unwrap(current);
        amount = isToken1
            ? amount1Delta(current, target, p.liquidity, true)
            : amount0Delta(current, target, p.liquidity, true);
    }

    function _swapBoth(Pools memory p, int32 tick) internal {
        (bool isToken1, uint256 amount) = _amountTo(p, tick);
        _swap(p.plain, isToken1, amount);
        _swap(p.v21, isToken1, amount);
    }

    /// Swaps `amount` through the executor, which keeps 1 wei so that the
    /// swap clears no balance.
    function _swap(PoolKey memory pk, bool isToken1, uint256 amount)
        internal
        returns (Vm.Gas memory gas, uint256 amountOut)
    {
        (address tokenIn, address tokenOut) =
            isToken1 ? (pk.token1, pk.token0) : (pk.token0, pk.token1);
        deal(tokenIn, address(executor), amount + 1);
        uint256 before = IERC20(tokenOut).balanceOf(SINK);
        executor.swap(
            amount,
            abi.encodePacked(tokenIn, tokenOut, PoolConfig.unwrap(pk.config)),
            SINK
        );
        gas = vm.lastCallGas();
        amountOut = IERC20(tokenOut).balanceOf(SINK) - before;
    }

    /// via-ir may reuse an earlier read of `block.timestamp`, so the block is
    /// read from the cheatcodes.
    function _nextBlock(uint256 secs) internal {
        vm.roll(vm.getBlockNumber() + 1);
        vm.warp(vm.getBlockTimestamp() + secs);
    }

    /// Records a swap that moves the price to about `tick`, after `warp`
    /// seconds (0: in the same block).
    function _rowTo(
        Recorder memory rec,
        Pools memory p,
        string memory name,
        int32 tick,
        uint256 warp
    ) internal {
        if (warp != 0) _nextBlock(warp);
        (bool isToken1, uint256 amount) = _amountTo(p, tick);
        _row(rec, p, name, isToken1, amount, 0);
    }

    function _row(
        Recorder memory rec,
        Pools memory p,
        string memory name,
        bool isToken1,
        uint256 amount,
        uint256 warp
    ) internal {
        if (warp != 0) _nextBlock(warp);
        string memory before = string.concat(
            "\"plainBefore\":",
            _state(p.plain),
            ",\"before\":",
            _state(p.v21),
            ",\"anchor\":\"",
            vm.toString(
                vm.load(MEV_CAPTURE_V21_ADDRESS, PoolId.unwrap(toPoolId(p.v21)))
            ),
            "\""
        );
        (Vm.Gas memory plain, uint256 plainOut) =
            _swap(p.plain, isToken1, amount);
        (Vm.Gas memory v21, uint256 v21Out) = _swap(p.v21, isToken1, amount);
        // Foundry versions disagree on whether the used gas includes refunds;
        // none of these swaps clears a slot.
        assertEq(plain.gasRefunded, 0, name);
        assertEq(v21.gasRefunded, 0, name);
        uint256 plainGas = plain.gasTotalUsed;
        uint256 v21Gas = v21.gasTotalUsed;

        if (rec.write) {
            rec.rows = string.concat(
                rec.rows,
                rec.count == 0 ? "" : ",\n",
                string.concat(
                    "{\"name\":\"",
                    name,
                    "\",\"pool\":\"",
                    p.name,
                    "\",\"now\":",
                    vm.toString(vm.getBlockTimestamp()),
                    ",\"isToken1\":",
                    isToken1 ? "true" : "false",
                    ",\"amount\":\"",
                    vm.toString(amount),
                    "\","
                ),
                before,
                string.concat(
                    ",\"plainGas\":",
                    vm.toString(plainGas),
                    ",\"v21Gas\":",
                    vm.toString(v21Gas),
                    ",\"plainOut\":\"",
                    vm.toString(plainOut),
                    "\",\"v21Out\":\"",
                    vm.toString(v21Out),
                    "\"}"
                )
            );
        } else {
            string memory key =
                string.concat(".rows[", vm.toString(rec.count), "]");
            string memory r = rec.recorded;
            assertEq(
                vm.parseJsonString(r, string.concat(key, ".name")), name, name
            );
            assertEq(
                vm.parseJsonUint(r, string.concat(key, ".amount")), amount, name
            );
            assertEq(
                vm.parseJsonUint(r, string.concat(key, ".plainOut")),
                plainOut,
                name
            );
            assertEq(
                vm.parseJsonUint(r, string.concat(key, ".v21Out")), v21Out, name
            );
            assertApproxEqRel(
                v21Gas - plainGas,
                vm.parseJsonUint(r, string.concat(key, ".v21Gas"))
                    - vm.parseJsonUint(r, string.concat(key, ".plainGas")),
                0.01e18,
                name
            );
        }
        rec.count++;
    }

    function _state(PoolKey memory pk) internal view returns (string memory) {
        PoolState state = CORE.poolState(toPoolId(pk));
        return string.concat(
            "{\"sqrtRatio\":\"",
            vm.toString(uint256(SqrtRatio.unwrap(state.sqrtRatio()))),
            "\",\"tick\":",
            vm.toString(int256(state.tick())),
            ",\"liquidity\":\"",
            vm.toString(uint256(state.liquidity())),
            "\"}"
        );
    }
}
