// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "@src/executors/LiquidityPartyExecutor.sol";
import {Constants} from "../Constants.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

contract LiquidityPartyExecutorExposed is LiquidityPartyExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            IPartyPool pool,
            address tokenIn,
            address tokenOut,
            uint8 indexIn,
            uint8 indexOut
        )
    {
        return _decodeData(data);
    }
}

contract LiquidityPartyExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    LiquidityPartyExecutorExposed liquidityPartyExposed;
    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 WETH = IERC20(WETH_ADDR);
    IERC20 AAVE = IERC20(AAVE_ADDR);

    // LiquidityParty pool address
    address constant LIQUIDITY_PARTY_POOL =
        0x1270Da05Cf1d047763CEEfDe25a4a5438b26fdA6;

    // Token indices in the pool
    uint8 constant USDC_INDEX = 0;
    uint8 constant WETH_INDEX = 1;
    uint8 constant AAVE_INDEX = 2;

    address internal constant AAVE_ADDR =
        address(0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9);

    // Mock pool address for decode testing
    address constant MOCK_POOL =
        address(0x1234567890123456789012345678901234567890);

    function setUp() public {
        uint256 forkBlock = 25301915;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        liquidityPartyExposed = new LiquidityPartyExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(
            MOCK_POOL, WETH_ADDR, USDC_ADDR, uint8(0), uint8(1)
        );

        (
            IPartyPool pool,
            address tokenIn,
            address tokenOut,
            uint8 indexIn,
            uint8 indexOut
        ) = liquidityPartyExposed.decodeParams(params);

        assertEq(address(pool), MOCK_POOL);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertEq(indexIn, 0);
        assertEq(indexOut, 1);
    }

    function testDecodeParamsCorrectLength() public view {
        // Verify that exactly 62 bytes works correctly
        bytes memory params = abi.encodePacked(
            MOCK_POOL, // 20 bytes
            WETH_ADDR, // 20 bytes
            USDC_ADDR, // 20 bytes
            uint8(0), // 1 byte
            uint8(1) // 1 byte
        );

        assertEq(params.length, 62);

        (
            IPartyPool pool,
            address tokenIn,
            address tokenOut,
            uint8 indexIn,
            uint8 indexOut
        ) = liquidityPartyExposed.decodeParams(params);

        assertEq(address(pool), MOCK_POOL);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertEq(indexIn, 0);
        assertEq(indexOut, 1);
    }

    function testDecodeParamsInvalidDataLength() public {
        // Data too short (only 40 bytes instead of 62)
        bytes memory invalidParams = abi.encodePacked(MOCK_POOL, WETH_ADDR);

        vm.expectRevert();
        liquidityPartyExposed.decodeParams(invalidParams);
    }

    function testDecodeParamsWithRealPool() public view {
        bytes memory params = abi.encodePacked(
            LIQUIDITY_PARTY_POOL, WETH_ADDR, AAVE_ADDR, WETH_INDEX, AAVE_INDEX
        );

        (
            IPartyPool pool,
            address tokenIn,
            address tokenOut,
            uint8 indexIn,
            uint8 indexOut
        ) = liquidityPartyExposed.decodeParams(params);

        assertEq(address(pool), LIQUIDITY_PARTY_POOL);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, AAVE_ADDR);
        assertEq(indexIn, WETH_INDEX);
        assertEq(indexOut, AAVE_INDEX);
    }

    /// @dev Mimics what the Dispatcher does in production: transfers tokens to the pool
    /// before calling swap(). We transfer via address(this) rather than using deal() directly
    /// on the pool, so the pool's internal balance tracking sees a real incoming transfer
    /// rather than a storage slot overwrite that would wipe its existing reserves.
    function _fundPool(address token, uint256 amount) internal {
        deal(token, address(this), amount);
        IERC20(token).safeTransfer(LIQUIDITY_PARTY_POOL, amount);
    }

    /// @dev Verifies that Rust-encoded calldata from test_encode_liquidityparty()
    /// (WETH → AAVE) executes correctly against a mainnet fork.
    function testDecodeSwap() public {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_liquidityparty");
        uint256 amountIn = 1e13; // 0.00001 WETH
        uint256 expectedAmountOut = 258564911454692; // AAVE out at fork block
        address expectedReceiver = makeAddr("decode_swap_receiver");
        _fundPool(WETH_ADDR, amountIn);
        liquidityPartyExposed.swap(amountIn, protocolData, expectedReceiver);

        assertGe(AAVE.balanceOf(expectedReceiver), expectedAmountOut);
    }

    function testSwapWETHToAAVE() public {
        uint256 amountIn = 1e13; // 0.00001 WETH
        uint256 expectedAmountOut = 258564911454692; // AAVE out at fork block
        bytes memory protocolData = abi.encodePacked(
            LIQUIDITY_PARTY_POOL, WETH_ADDR, AAVE_ADDR, WETH_INDEX, AAVE_INDEX
        );

        uint256 balanceBefore = AAVE.balanceOf(BOB);
        _fundPool(WETH_ADDR, amountIn);
        liquidityPartyExposed.swap(amountIn, protocolData, BOB);

        assertGe(AAVE.balanceOf(BOB) - balanceBefore, expectedAmountOut);
    }

    function testSwapAAVEToWETH() public {
        uint256 amountIn = 1e13; // 0.00001 AAVE (18 decimals)
        uint256 expectedAmountOut = 384925572039; // WETH out at fork block
        bytes memory protocolData = abi.encodePacked(
            LIQUIDITY_PARTY_POOL, AAVE_ADDR, WETH_ADDR, AAVE_INDEX, WETH_INDEX
        );

        uint256 balanceBefore = WETH.balanceOf(BOB);
        _fundPool(AAVE_ADDR, amountIn);
        liquidityPartyExposed.swap(amountIn, protocolData, BOB);

        assertGe(WETH.balanceOf(BOB) - balanceBefore, expectedAmountOut);
    }

    function testSwapUSDCToWETH() public {
        uint256 amountIn = 10000; // 0.01 USDC (6 decimals)
        uint256 expectedAmountOut = 6002311486634; // WETH out at fork block
        bytes memory protocolData = abi.encodePacked(
            LIQUIDITY_PARTY_POOL, USDC_ADDR, WETH_ADDR, USDC_INDEX, WETH_INDEX
        );

        uint256 balanceBefore = WETH.balanceOf(BOB);
        _fundPool(USDC_ADDR, amountIn);
        liquidityPartyExposed.swap(amountIn, protocolData, BOB);

        assertGe(WETH.balanceOf(BOB) - balanceBefore, expectedAmountOut);
    }
}
