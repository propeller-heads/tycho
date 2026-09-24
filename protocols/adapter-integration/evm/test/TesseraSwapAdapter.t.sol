// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.27;
import "forge-std/Test.sol";
import {
    TesseraSwapAdapter,
    ITesseraSwap
} from "src/tessera/TesseraSwapAdapter.sol";
import {ISwapAdapterTypes} from "src/interfaces/ISwapAdapterTypes.sol";
import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

contract LimitedTesseraQuotes {
    function tesseraSwapViewAmounts(address, address, int256 amount)
        external
        pure
        returns (uint256, uint256)
    {
        uint256 input =
            amount < 0 ? (uint256(-amount) + 1) / 2 : uint256(amount);
        require(input > 0 && input <= 1e15, "ladder depleted");
        return (input, amount < 0 ? uint256(-amount) : input * 2);
    }
}

contract TesseraSwapAdapterTest is Test {
    address constant VENUE = 0x55555522005BcAE1c2424D474BfD5ed477749E3e;
    address constant PAIR = 0xf524C1Bc1C64A2C99bc7eccf19EDe9a1d89d5a7C;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    TesseraSwapAdapter adapter;
    bytes32 poolId = bytes32(bytes20(PAIR));

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 50_548_423);
        adapter = new TesseraSwapAdapter(VENUE);
    }

    function test_tokens_capabilities_and_limits() public {
        address[] memory tokens = adapter.getTokens(poolId);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);
        assertEq(adapter.getCapabilities(poolId, WETH, USDC).length, 4);
        for (uint256 i; i < 2; ++i) {
            (address sell, address buy) = i == 0 ? (WETH, USDC) : (USDC, WETH);
            uint256 gasBefore = gasleft();
            uint256[] memory limits = adapter.getLimits(poolId, sell, buy);
            uint256 used = gasBefore - gasleft();
            emit log_named_uint("getLimits gas", used);
            assertLe(used, 3_000_000, "getLimits budget");
            assertGt(limits[0], 0);
            assertGt(limits[1], 0);
            (, uint256 quoted) = ITesseraSwap(VENUE)
                .tesseraSwapViewAmounts(sell, buy, int256(limits[0]));
            assertEq(limits[1], quoted);
        }
    }

    function _swap(
        address sell,
        address buy,
        ISwapAdapterTypes.OrderSide side,
        uint256 amount
    ) internal {
        int256 specified = side == ISwapAdapterTypes.OrderSide.Sell
            ? int256(amount)
            : -int256(amount);
        (uint256 input, uint256 output) =
            ITesseraSwap(VENUE).tesseraSwapViewAmounts(sell, buy, specified);
        assertGt(output, 0);
        deal(sell, address(this), input);
        IERC20(sell).approve(address(adapter), input);
        uint256 beforeOut = IERC20(buy).balanceOf(address(this));
        ISwapAdapterTypes.Trade memory trade =
            adapter.swap(poolId, sell, buy, side, amount);
        assertEq(IERC20(buy).balanceOf(address(this)) - beforeOut, output);
        assertEq(
            trade.calculatedAmount,
            side == ISwapAdapterTypes.OrderSide.Sell ? output : input
        );
    }

    function test_exact_input_both_directions() public {
        _swap(WETH, USDC, ISwapAdapterTypes.OrderSide.Sell, 0.01 ether);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
    }

    function test_exact_output_both_directions() public {
        _swap(WETH, USDC, ISwapAdapterTypes.OrderSide.Buy, 100e6);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Buy, 0.01 ether);
    }

    function test_two_fills_reuse_mutated_pair_state() public {
        uint256 beforeAccumulator = uint256(vm.load(PAIR, bytes32(uint256(3))));
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
        assertGt(uint256(vm.load(PAIR, bytes32(uint256(3)))), beforeAccumulator);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
    }

    function test_price_and_staleness_use_block_number() public {
        uint256[] memory amounts = new uint256[](1);
        assertGt(adapter.price(poolId, WETH, USDC, amounts)[0].numerator, 0);
        vm.roll(block.number + 100);
        assertEq(adapter.price(poolId, WETH, USDC, amounts)[0].numerator, 0);
    }

    function test_limits_preserve_small_remaining_liquidity() public {
        // Keep the real pair's large ladder bound but allow only a small quote.
        vm.etch(VENUE, address(new LimitedTesseraQuotes()).code);
        uint256[] memory limits = adapter.getLimits(poolId, WETH, USDC);
        assertGt(limits[0], 0);
        assertLe(limits[0], 1e15);
        assertGt(limits[1], 0);
        assertLe(limits[1], limits[0] * 2);
    }

    function test_cbbtc_limits_and_price_both_directions() public {
        address btc = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
        bytes32 btcPool = bytes32(
            bytes20(address(0xED57BacDc2a990B631F8817853935791C122c356))
        );
        for (uint256 i; i < 2; ++i) {
            (address sell, address buy) = i == 0 ? (USDC, btc) : (btc, USDC);
            uint256 gasBefore = gasleft();
            uint256[] memory limits = adapter.getLimits(btcPool, sell, buy);
            emit log_named_uint("cbBTC getLimits gas", gasBefore - gasleft());
            assertLe(gasBefore - gasleft(), 3_000_000);
            assertGt(limits[0], 0);
            assertGt(limits[1], 0);
            (, uint256 output) = ITesseraSwap(VENUE)
                .tesseraSwapViewAmounts(sell, buy, int256(limits[0]));
            assertGe(output, limits[1]);
            uint256[] memory amounts = new uint256[](1);
            assertGt(adapter.price(btcPool, sell, buy, amounts)[0].numerator, 0);
        }
    }

    function test_rejects_wrong_tokens() public {
        vm.expectRevert();
        adapter.getLimits(poolId, WETH, address(1));
    }
}
