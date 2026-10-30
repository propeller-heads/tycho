pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/PancakeswapInfinityExecutor.sol";
import "@src/executors/PancakeswapInfinityRobinhoodExecutor.sol";
import {Constants} from "../Constants.sol";

address constant INFINITY_VAULT = 0x238a358808379702088667322f80aC48bAd5e6c4;
address constant INFINITY_CL_POOL_MANAGER =
    0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b;
address constant INFINITY_BIN_POOL_MANAGER =
    0xC697d2898e0D09264376196696c51D7aBbbAA4a9;
address constant BASE_USDT = 0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2;
// The USDC/USDT CL and Bin pools and the ETH/USDC Bin pool all have liquidity at this block.
uint256 constant INFINITY_FORK_BLOCK = 51600000;

// Robinhood has a Vault and CL pool manager but no BinPoolManager.
address constant ROBINHOOD_WETH = 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
address constant ROBINHOOD_USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;
// The WETH/USDG CL pool (fee 90, tick spacing 1, no hooks) has liquidity at this block.
uint256 constant ROBINHOOD_FORK_BLOCK = 91076200;

// The 97 bytes `_decodeData` expects for the live USDC/USDT CL pool: fee 5, tick spacing 2 in
// bits [16,40) of `parameters`, no hooks. Same field order as the Rust encoder.
function usdcUsdtSwapData(address tokenIn, address tokenOut, bool zeroForOne)
    pure
    returns (bytes memory)
{
    return abi.encodePacked(
        tokenIn,
        tokenOut,
        zeroForOne,
        uint8(0), // CL
        uint24(5),
        bytes32(uint256(2) << 16),
        address(0)
    );
}

/// A live Base Bin pool with bin step 10 and no hook: USDC/USDT has `lpFee` 7, ETH/USDC 201.
/// `parameters` packs the bin step above the hook bitmap, the same slot the CL pool uses for tick
/// spacing.
function binSwapData(
    address tokenIn,
    address tokenOut,
    bool zeroForOne,
    uint24 lpFee
) pure returns (bytes memory) {
    return abi.encodePacked(
        tokenIn,
        tokenOut,
        zeroForOne,
        uint8(1), // Bin
        lpFee,
        bytes32(uint256(10) << 16),
        address(0)
    );
}

contract PancakeswapInfinityExecutorExposed is PancakeswapInfinityExecutor {
    constructor(address vault_, address clPoolManager_, address binPoolManager_)
        PancakeswapInfinityExecutor(
            IPancakeswapInfinityVault(vault_), clPoolManager_, binPoolManager_
        )
    {}

    using SafeERC20 for IERC20;

    /// @dev Stands in for Dispatcher._callHandleCallbackOnExecutor, which moves the input to the
    /// Vault mid-callback. This contract took the lock, so msg.sender is the Vault and
    /// `_handleCallback` is reachable internally. Payload is fixed width: in `lockAcquired(bytes)`
    /// calldata it starts at 68, amountIn first, then tokenIn at payload[52].
    fallback(bytes calldata) external returns (bytes memory) {
        uint256 amountIn = uint256(bytes32(msg.data[68:100]));
        address tokenIn = address(bytes20(msg.data[120:140]));

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType == TransferManager.TransferType.Transfer) {
            IERC20(tokenIn).safeTransfer(receiver, amountIn);
        }
        return _handleCallback(msg.data);
    }

    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 parameters,
            address hooks
        )
    {
        return _decodeData(data);
    }
}

contract PancakeswapInfinityExecutorTest is Constants, TestUtils {
    PancakeswapInfinityExecutorExposed executor;

    function setUp() public {
        executor = new PancakeswapInfinityExecutorExposed(
            INFINITY_VAULT, INFINITY_CL_POOL_MANAGER, INFINITY_BIN_POOL_MANAGER
        );
    }

    function testDecodeParams() public view {
        bytes32 parameters = bytes32(uint256(60) << 16);
        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_USDC,
            true,
            uint8(0),
            uint24(500),
            parameters,
            address(0)
        );
        (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 decodedParameters,
            address hooks
        ) = executor.decodeParams(data);
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertTrue(zeroForOne, "direction byte 1 decodes as zeroForOne");
        assertEq(poolType, 0);
        assertEq(fee, 500);
        assertEq(decodedParameters, parameters);
        assertEq(hooks, address(0));
    }

    function testDecodeParamsRejectsUnknownPoolType() public {
        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_USDC,
            true,
            uint8(2),
            uint24(500),
            bytes32(0),
            address(0)
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                PancakeswapInfinityExecutor__UnknownPoolType.selector, uint8(2)
            )
        );
        executor.decodeParams(data);
    }

    function testDecodeParamsRejectsWrongLength() public {
        vm.expectRevert(PancakeswapInfinityExecutor__InvalidDataLength.selector);
        executor.decodeParams(hex"deadbeef");
    }

    function testCallbackRejectsNonVault() public {
        vm.expectRevert(PancakeswapInfinityExecutor__NotVault.selector);
        executor.handleCallback(new bytes(68));
    }

    /// The payload is read at fixed offsets, so a callback that is not lockAcquired has to be
    /// rejected rather than misparsed into a pool key.
    function testCallbackRejectsUnknownSelector() public {
        vm.prank(INFINITY_VAULT);
        vm.expectRevert(
            abi.encodeWithSelector(
                PancakeswapInfinityExecutor__UnexpectedCallback.selector,
                bytes4(0)
            )
        );
        executor.handleCallback(new bytes(68));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(
            usdcUsdtSwapData(BASE_USDC, BASE_USDT, true)
        );
        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, BASE_USDT);
        assertFalse(outputToRouter, "the Vault pays the receiver directly");
    }

    function testGetCallbackTransferDataErc20() public view {
        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData("", BASE_USDC, address(0));
        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, INFINITY_VAULT);
    }

    function testGetCallbackTransferDataNative() public view {
        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData("", ETH_ADDRESS, address(0));
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
    }

    function testFundsExpectedAddressIsTheCaller() public view {
        assertEq(executor.fundsExpectedAddress(""), address(this));
    }
}

/**
 * Router-level setup check: the executor deploys and is whitelisted with the others in
 * TychoRouterTestSetup.deployExecutors, at the deterministic address committed in
 * config/test_executor_addresses.json under "pancakeswap_infinity_cl"; the Rust registry
 * resolves "pancakeswap_infinity_bin" to the same executor, so there is no second entry.
 */
contract TychoRouterForPancakeswapInfinityTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return INFINITY_FORK_BLOCK;
    }

    function testExecutorDeployedAndWhitelisted() public view {
        address executor = address(pancakeswapInfinityExecutor);

        assertTrue(executor != address(0), "executor is not deployed");
        assertTrue(
            tychoRouter.executorsActivationTimestamp(executor) != 0,
            "executor is not whitelisted on the router"
        );
        // Pinned: deployExecutors moves every CREATE address after the executor it
        // inserts, so a reordering has to fail here rather than silently stale the
        // address the Rust encoder tests read.
        assertEq(
            executor,
            0xe54a55121A47451c5727ADBAF9b9FC1643477e25,
            "executor address drifted from test_executor_addresses.json"
        );
    }

    /// A CL swap through the Dispatcher, which measures the output at the receiver.
    /// PancakeswapInfinityForkTest calls the executor directly through a stand-in instead.
    function testSingleSwap() public {
        uint256 amountIn = 1000e6;
        uint256 expAmountOut = 1000121097;
        deal(BASE_USDC, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDT).balanceOf(ALICE);

        bytes memory swap = encodeSingleSwap(
            address(pancakeswapInfinityExecutor),
            usdcUsdtSwapData(BASE_USDC, BASE_USDT, true)
        );

        vm.startPrank(ALICE);
        IERC20(BASE_USDC).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            BASE_USDC,
            BASE_USDT,
            expAmountOut,
            expAmountOut,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();

        assertEq(amountOut, expAmountOut);
        assertEq(
            IERC20(BASE_USDT).balanceOf(ALICE) - balanceBefore, expAmountOut
        );
        // outputToRouter is false, so the router must never hold either token.
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDT).balanceOf(tychoRouterAddr), 0);
    }

    /// Same path with poolType 1, which routes the swap to the Bin manager instead. The Bin pool
    /// is shallow at this block, so the input is 1 USDC rather than the CL case's 1000.
    function testSingleSwapBin() public {
        uint256 amountIn = 1e6;
        uint256 expAmountOut = 1000026;
        deal(BASE_USDC, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDT).balanceOf(ALICE);

        bytes memory swap = encodeSingleSwap(
            address(pancakeswapInfinityExecutor),
            binSwapData(BASE_USDC, BASE_USDT, true, 7)
        );

        vm.startPrank(ALICE);
        IERC20(BASE_USDC).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            BASE_USDC,
            BASE_USDT,
            expAmountOut,
            expAmountOut,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();

        assertEq(amountOut, expAmountOut);
        assertEq(
            IERC20(BASE_USDT).balanceOf(ALICE) - balanceBefore, expAmountOut
        );
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDT).balanceOf(tychoRouterAddr), 0);
    }

    /// Both native legs on the only live pool that can serve them. Native is `address(0)` inside
    /// Infinity and ETH_ADDRESS everywhere else, so this and testSequentialClToBinIntegration are
    /// the coverage of `settle{value:}`, `take(address(0), ..)` and TransferNativeInExecutor. No
    /// bin at or below the active one holds USDC at this block, so USDC -> ETH has to run first:
    /// the USDC it leaves in the bins it crosses is the only liquidity ETH -> USDC can take, hence
    /// the second leg buys back half of what the first sold. ETH goes to BOB because ALICE is a
    /// hardcoded address that carries an EIP-7702 delegation on Base which forwards any ETH it
    /// receives, so its balance diff would read zero.
    function testSingleSwapBinNativeOutThenIn() public {
        uint256 usdcIn = 100_000;
        uint256 expEthOut = 30416960559745;
        deal(BASE_USDC, ALICE, usdcIn);
        uint256 ethBefore = BOB.balance;

        vm.startPrank(ALICE);
        IERC20(BASE_USDC).approve(tychoRouterAddr, usdcIn);
        uint256 ethOut = tychoRouter.singleSwap(
            usdcIn,
            BASE_USDC,
            ETH_ADDRESS,
            expEthOut,
            expEthOut,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                address(pancakeswapInfinityExecutor),
                binSwapData(BASE_USDC, ETH_ADDRESS, false, 201)
            )
        );
        vm.stopPrank();

        assertEq(ethOut, expEthOut);
        assertEq(BOB.balance - ethBefore, expEthOut, "BOB did not get the ETH");

        uint256 ethIn = expEthOut / 2;
        uint256 expUsdcOut = 49_999;
        uint256 usdcBefore = IERC20(BASE_USDC).balanceOf(BOB);

        // The native leg is encoded by the Rust TychoRouterEncoder.
        vm.prank(BOB);
        (bool success,) = tychoRouterAddr.call{value: ethIn}(
            loadCallDataFromFile(
                "test_single_encoding_strategy_pancakeswap_infinity_bin_eth_in"
            )
        );

        assertTrue(success, "Call Failed");
        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB) - usdcBefore,
            expUsdcOut,
            "BOB did not get the USDC"
        );
        assertEq(tychoRouterAddr.balance, 0, "router kept ETH");
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
    }

    /// USDT -> (CL) -> USDC -> (Bin) -> ETH, encoded by the Rust TychoRouterEncoder. Two Infinity
    /// hops take two vault locks back to back, the CL hop's USDC must stay in the router for the
    /// Bin hop (fundsExpectedAddress), and the Bin hop pays out native ETH.
    function testSequentialClToBinIntegration() public {
        deal(BASE_USDT, ALICE, 100_000);
        uint256 ethBefore = BOB.balance;

        vm.startPrank(ALICE);
        IERC20(BASE_USDT).approve(tychoRouterAddr, 100_000);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_sequential_encoding_strategy_pancakeswap_infinity_cl_to_bin"
            )
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        // The Rust solution's expected and minimum output, so the route fills exactly.
        assertEq(
            BOB.balance - ethBefore, 30412401940077, "BOB did not get the ETH"
        );
        assertEq(IERC20(BASE_USDT).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0, "router kept ETH");
    }
}

contract PancakeswapInfinityForkTest is Constants, TestUtils {
    PancakeswapInfinityExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), INFINITY_FORK_BLOCK);
        executor = new PancakeswapInfinityExecutorExposed(
            INFINITY_VAULT, INFINITY_CL_POOL_MANAGER, INFINITY_BIN_POOL_MANAGER
        );
    }

    /// @dev Swaps `amountIn` through the executor and pins the exact output at ALICE. The Bin
    /// pool is far shallower than the CL one at this block, so its cases size the input
    /// themselves rather than draining the pool and reverting BinPool__OutOfLiquidity.
    function _assertSwap(
        address tokenIn,
        address tokenOut,
        bytes memory data,
        uint256 amountIn,
        uint256 expectedOut
    ) internal {
        deal(tokenIn, address(executor), amountIn);
        executor.swap(amountIn, data, ALICE);
        assertEq(IERC20(tokenOut).balanceOf(ALICE), expectedOut);
        assertEq(IERC20(tokenIn).balanceOf(address(executor)), 0);
    }

    function testSwapUsdcToUsdt() public {
        _assertSwap(
            BASE_USDC,
            BASE_USDT,
            usdcUsdtSwapData(BASE_USDC, BASE_USDT, true),
            1000e6,
            1000121097
        );
    }

    function testSwapUsdtToUsdc() public {
        _assertSwap(
            BASE_USDT,
            BASE_USDC,
            usdcUsdtSwapData(BASE_USDT, BASE_USDC, false),
            1000e6,
            999843095
        );
    }

    /// The Rust encoder's bytes are the hand-built ones, and they swap.
    function testSwapMatchesEncoderCalldata() public {
        bytes memory data =
            loadCallDataFromFile("test_encode_pancakeswap_infinity_cl_swap");
        assertEq(data, usdcUsdtSwapData(BASE_USDC, BASE_USDT, true));
        _assertSwap(BASE_USDC, BASE_USDT, data, 1000e6, 1000121097);
    }

    function testSwapBinUsdcToUsdt() public {
        _assertSwap(
            BASE_USDC,
            BASE_USDT,
            binSwapData(BASE_USDC, BASE_USDT, true, 7),
            1e6,
            1000026
        );
    }

    function testSwapBinUsdtToUsdc() public {
        _assertSwap(
            BASE_USDT,
            BASE_USDC,
            binSwapData(BASE_USDT, BASE_USDC, false, 7),
            1e6,
            998991
        );
    }

    /// More input than every bin on the output side holds: the pool reverts rather than fill
    /// part of it.
    /// https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/BinPool.sol#L191-L197
    function testSwapBinRevertsPastLiquidity() public {
        uint256 amountIn = 1_000_000e6;
        deal(BASE_USDC, address(executor), amountIn);

        vm.expectRevert(bytes4(keccak256("BinPool__OutOfLiquidity()")));
        executor.swap(
            amountIn, binSwapData(BASE_USDC, BASE_USDT, true, 7), ALICE
        );
    }

    /// The Rust encoder's bytes are the hand-built ones, and they swap.
    function testSwapBinMatchesEncoderCalldata() public {
        bytes memory data =
            loadCallDataFromFile("test_encode_pancakeswap_infinity_bin_swap");
        assertEq(data, binSwapData(BASE_USDC, BASE_USDT, true, 7));
        _assertSwap(BASE_USDC, BASE_USDT, data, 1e6, 1000026);
    }
}

/// Same callback stand-in as PancakeswapInfinityExecutorExposed.
contract PancakeswapInfinityRobinhoodExecutorExposed is
    PancakeswapInfinityRobinhoodExecutor
{
    using SafeERC20 for IERC20;

    constructor(address vault_, address clPoolManager_, address binPoolManager_)
        PancakeswapInfinityRobinhoodExecutor(
            IPancakeswapInfinityVault(vault_), clPoolManager_, binPoolManager_
        )
    {}

    fallback(bytes calldata) external returns (bytes memory) {
        uint256 amountIn = uint256(bytes32(msg.data[68:100]));
        address tokenIn = address(bytes20(msg.data[120:140]));

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType == TransferManager.TransferType.Transfer) {
            IERC20(tokenIn).safeTransfer(receiver, amountIn);
        }
        return _handleCallback(msg.data);
    }
}

/// Runs the executor as `executor_deployments.json` deploys it on Robinhood.
contract PancakeswapInfinityRobinhoodForkTest is Constants, TestUtils {
    PancakeswapInfinityRobinhoodExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("robinhood"), ROBINHOOD_FORK_BLOCK);
        address[] memory args = vm.parseJsonAddressArray(
            vm.readFile("../config/executor_deployments.json"),
            ".robinhood.pancakeswap_infinity_cl.args"
        );
        executor = new PancakeswapInfinityRobinhoodExecutorExposed(
            args[0], args[1], args[2]
        );
    }

    function testConstructorRequiresAManager() public {
        vm.expectRevert(
            PancakeswapInfinityRobinhoodExecutor__ZeroAddress.selector
        );
        new PancakeswapInfinityRobinhoodExecutorExposed(
            INFINITY_VAULT, address(0), address(0)
        );
    }

    function testSwapWethToUsdg() public {
        uint256 amountIn = 0.01 ether;
        deal(ROBINHOOD_WETH, address(executor), amountIn);
        executor.swap(
            amountIn,
            abi.encodePacked(
                ROBINHOOD_WETH,
                ROBINHOOD_USDG,
                true, // zeroForOne
                uint8(0), // CL
                uint24(90), // fee
                bytes32(uint256(1) << 16), // tick spacing 1
                address(0) // no hooks
            ),
            ALICE
        );
        assertGt(IERC20(ROBINHOOD_USDG).balanceOf(ALICE), 0);
        assertEq(IERC20(ROBINHOOD_WETH).balanceOf(address(executor)), 0);
    }

    function testBinSwapReverts() public {
        uint256 amountIn = 0.01 ether;
        deal(ROBINHOOD_WETH, address(executor), amountIn);
        // Reverts before any pool is looked up, so fee and parameters are arbitrary.
        bytes memory binData = abi.encodePacked(
            ROBINHOOD_WETH,
            ROBINHOOD_USDG,
            true, // zeroForOne
            uint8(1), // Bin
            uint24(90), // fee
            bytes32(uint256(10) << 16), // bin step 10
            address(0) // no hooks
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                PancakeswapInfinityRobinhoodExecutor__UnknownPoolType.selector,
                uint8(1)
            )
        );
        executor.swap(amountIn, binData, ALICE);
    }
}
