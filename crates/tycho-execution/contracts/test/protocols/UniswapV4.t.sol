pragma solidity ^0.8.26;

import "../../src/executors/UniswapV4Executor.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "./UniswapV4Utils.sol";
import "@src/executors/UniswapV4Executor.sol";
import {Constants} from "../Constants.sol";
import {SafeCallback} from "@uniswap/v4-periphery/src/base/SafeCallback.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";

contract UniswapV4ExecutorExposed is UniswapV4Executor {
    using SafeERC20 for IERC20;
    constructor(IPoolManager _POOL_MANAGER, address _ANGSTROM_HOOK)
        UniswapV4Executor(_POOL_MANAGER, _ANGSTROM_HOOK)
    {}

    function decodeData(bytes calldata data)
        external
        view
        returns (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            bool skipUnlock,
            UniswapV4Pool[] memory pools
        )
    {
        return _decodeData(data);
    }

    fallback(
        bytes calldata /*data*/
    )
        external
        returns (bytes memory)
    {
        // Extract tokenIn from the unlock callback data (msg.data[68:] = swapData).
        // swapExactInputSingle ABI layout: selector(4) | currency0(32) | currency1(32) | ...
        //   | zeroForOne(32) | ...  → tokenIn = zeroForOne ? currency0 : currency1
        // swapExactInput ABI layout: selector(4) | currencyIn(32) | ...
        //   → tokenIn = currencyIn (address occupies last 20 bytes of the 32-byte slot)
        bytes calldata stripped = msg.data[68:];
        bytes4 sel = bytes4(stripped[:4]);
        address tokenIn;
        if (sel == this.swapExactInput.selector) {
            tokenIn = address(bytes20(stripped[16:36]));
        } else {
            // swapExactInputSingle: PoolKey is static, currency0 at [4:36], currency1 at [36:68]
            // zeroForOne is at stripped[164:196], bool value at the last byte [195]
            address currency0 = address(bytes20(stripped[16:36]));
            address currency1 = address(bytes20(stripped[48:68]));
            bool zeroForOne = stripped[195] != 0;
            tokenIn = zeroForOne ? currency0 : currency1;
        }

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType == TransferManager.TransferType.Transfer) {
            uint256 amount;
            if (sel == this.swapExactInputSingle.selector) {
                // amountIn occupies stripped[196:228]; value in last 16 bytes
                amount = uint128(bytes16(stripped[212:228]));
            } else {
                // swapExactInput: amountIn at stripped[36:68]; value in last 16 bytes
                amount = uint128(bytes16(stripped[52:68]));
            }
            IERC20(tokenIn).safeTransfer(receiver, amount);
        }
        return abi.encode(_unlockCallback(stripped));
    }
}

contract UniswapV4ExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    UniswapV4ExecutorExposed uniswapV4Exposed;
    IERC20 USDE = IERC20(USDE_ADDR);
    IERC20 USDT = IERC20(USDT_ADDR);
    IERC20 USDC = IERC20(USDC_ADDR);

    function setUp() public {
        uint256 forkBlock = 22689128;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        uniswapV4Exposed = new UniswapV4ExecutorExposed(
            IPoolManager(POOL_MANAGER), ANGSTROM_HOOK
        );
    }

    function testDecodeParams() public view {
        bool zeroForOne = true;
        uint24 pool1Fee = 500;
        int24 tickSpacing1 = 60;
        uint24 pool2Fee = 1000;
        int24 tickSpacing2 = -10;

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: pool1Fee,
            tickSpacing: tickSpacing1,
            hook: address(0),
            hookData: bytes("")
        });
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDE_ADDR,
            fee: pool2Fee,
            tickSpacing: tickSpacing2,
            hook: address(0),
            hookData: bytes("0x12345")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, USDT_ADDR, zeroForOne, false, pools
        );

        (
            address tokenIn,
            address tokenOut,
            bool zeroForOneDecoded,
            bool skipUnlockDecoded,
            UniswapV4Executor.UniswapV4Pool[] memory decodedPools
        ) = uniswapV4Exposed.decodeData(data);

        assertEq(tokenIn, USDE_ADDR);
        assertEq(tokenOut, USDT_ADDR);
        assertEq(zeroForOneDecoded, zeroForOne);
        assertEq(skipUnlockDecoded, false);
        assertEq(decodedPools[0].hook, address(0));
        assertEq(decodedPools.length, 2);
        assertEq(decodedPools[0].intermediaryToken, USDT_ADDR);
        assertEq(decodedPools[0].fee, pool1Fee);
        assertEq(decodedPools[0].tickSpacing, tickSpacing1);
        assertEq(decodedPools[1].intermediaryToken, USDE_ADDR);
        assertEq(decodedPools[1].fee, pool2Fee);
        assertEq(decodedPools[1].tickSpacing, tickSpacing2);
        assertEq(decodedPools[1].hookData, bytes("0x12345"));
    }

    function testSingleSwap() public {
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, address(uniswapV4Exposed), amountIn);
        uint256 usdeBalanceBeforePool = USDE.balanceOf(POOL_MANAGER);
        uint256 usdeBalanceBeforeSwapExecutor =
            USDE.balanceOf(address(uniswapV4Exposed));

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, USDT_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertEq(USDE.balanceOf(POOL_MANAGER), usdeBalanceBeforePool + amountIn);
        assertEq(
            USDE.balanceOf(address(uniswapV4Exposed)),
            usdeBalanceBeforeSwapExecutor - amountIn
        );
        assertTrue(USDT.balanceOf(ALICE) > 0);
    }

    function testSingleSwapIntegration() public {
        // USDE -> USDT
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_uniswap_v4_simple_swap");
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, address(uniswapV4Exposed), amountIn);
        uint256 usdeBalanceBeforePool = USDE.balanceOf(POOL_MANAGER);
        uint256 usdeBalanceBeforeSwapExecutor =
            USDE.balanceOf(address(uniswapV4Exposed));

        uniswapV4Exposed.swap(amountIn, protocolData, ALICE);
        assertEq(USDE.balanceOf(POOL_MANAGER), usdeBalanceBeforePool + amountIn);
        assertEq(
            USDE.balanceOf(ALICE), usdeBalanceBeforeSwapExecutor - amountIn
        );
        assertTrue(USDT.balanceOf(ALICE) > 0);
    }

    function testMultipleSwap() public {
        // USDE -> USDT -> WBTC
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, address(uniswapV4Exposed), amountIn);
        uint256 usdeBalanceBeforePool = USDE.balanceOf(POOL_MANAGER);
        uint256 usdeBalanceBeforeSwapExecutor =
            USDE.balanceOf(address(uniswapV4Exposed));

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WBTC_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, WBTC_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertEq(USDE.balanceOf(POOL_MANAGER), usdeBalanceBeforePool + amountIn);
        assertEq(
            USDE.balanceOf(address(uniswapV4Exposed)),
            usdeBalanceBeforeSwapExecutor - amountIn
        );
        assertTrue(IERC20(WBTC_ADDR).balanceOf(ALICE) > 0);
    }

    function testMultipleSwapIntegration() public {
        // USDE -> USDT -> WBTC
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_uniswap_v4_sequential_swap");

        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, address(uniswapV4Exposed), amountIn);
        uint256 usdeBalanceBeforePool = USDE.balanceOf(POOL_MANAGER);
        uint256 usdeBalanceBeforeSwapExecutor =
            USDE.balanceOf(address(uniswapV4Exposed));

        uniswapV4Exposed.swap(amountIn, protocolData, ALICE);
        assertEq(USDE.balanceOf(POOL_MANAGER), usdeBalanceBeforePool + amountIn);
        assertEq(
            USDE.balanceOf(address(uniswapV4Exposed)),
            usdeBalanceBeforeSwapExecutor - amountIn
        );
        assertTrue(IERC20(WBTC_ADDR).balanceOf(ALICE) > 0);
    }

    function testSingleSwapEulerHook() public {
        // Replicating tx: 0xb372306a81c6e840f4ec55f006da6b0b097f435802a2e6fd216998dd12fb4aca
        address hook = address(0x69058613588536167BA0AA94F0CC1Fe420eF28a8);

        uint256 amountIn = 7407000000;
        deal(USDC_ADDR, address(uniswapV4Exposed), amountIn);
        uint256 usdcBalanceBeforeSwapExecutor =
            USDC.balanceOf(address(uniswapV4Exposed));

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WETH_ADDR,
            fee: uint24(500),
            tickSpacing: int24(1),
            hook: hook,
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDC_ADDR, WETH_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        uint256 amountOut = IERC20(WETH_ADDR).balanceOf(ALICE);
        assertEq(amountOut, 2681115183499232721);
        assertEq(
            USDC.balanceOf(address(uniswapV4Exposed)),
            usdcBalanceBeforeSwapExecutor - amountIn
        );
    }
}

contract UniswapV4ExecutorTestForEuler is Constants, TestUtils {
    /* These tests are necessary because Euler works a little differently from general UniswapV4 logic.
    In the previous version of the UniswapV4Executor we are only sending the user's tokens into the Pool Manager
    after we call swap on it. This is ok because the Pool Manager tracks the debts and accepts everything as long
    as the tokens are transfers inside of the unlock callback. However, Euler expects the funds to already be
    in the Pool Manager when beforeSwap is called. This is not a problem for tokens that the Pool Manager has a
    lot of, but for tokens with low balances this makes the tx fail. We need to transfer the tokens into
    the Pool Manager before we call swap on it.
    The only risk here is that we are assuming that the amount_in will never change. In the previous version, we
    were confirming this amount with the currencyDelta of the Pool Manager. Now we pray.
    */
    using SafeERC20 for IERC20;

    UniswapV4ExecutorExposed uniswapV4Exposed;
    IERC20 USDT = IERC20(USDT_ADDR);
    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 RLUSD = IERC20(RLUSD_ADDR);
    IERC20 WBTC = IERC20(WBTC_ADDR);

    function setUp() public {
        uint256 forkBlock = 23535338;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        uniswapV4Exposed = new UniswapV4ExecutorExposed(
            IPoolManager(POOL_MANAGER), ANGSTROM_HOOK
        );
    }

    function testSingleSwapEulerLowBalance() public {
        uint256 amountIn = 134187695711754971245517404;
        deal(RLUSD_ADDR, address(uniswapV4Exposed), amountIn);
        address eulerProxy = 0xe1Ce9AF672f8854845E5474400B6ddC7AE458a10;
        uint256 rlusdEulerBalanceBefore = RLUSD.balanceOf(eulerProxy);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(50),
            tickSpacing: int24(1),
            hook: address(0xF87ACF8428F2f9403AAA0256A7272d6549ECa8A8),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            RLUSD_ADDR, USDT_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertEq(
            RLUSD.balanceOf(eulerProxy), rlusdEulerBalanceBefore + amountIn
        );
        assertTrue(USDT.balanceOf(ALICE) > 0);
    }

    function testMultipleSwapEulerLowBalance() public {
        // RLUSD -(euler)-> USDT -> WBTC
        uint256 amountIn = 134187695711754971245517404;
        deal(RLUSD_ADDR, address(uniswapV4Exposed), amountIn);
        address eulerProxy = 0xe1Ce9AF672f8854845E5474400B6ddC7AE458a10;
        uint256 rlusdEulerBalanceBefore = RLUSD.balanceOf(eulerProxy);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(50),
            tickSpacing: int24(1),
            hook: address(0xF87ACF8428F2f9403AAA0256A7272d6549ECa8A8),
            hookData: bytes("")
        });
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WBTC_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            RLUSD_ADDR, WBTC_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertEq(
            RLUSD.balanceOf(eulerProxy), rlusdEulerBalanceBefore + amountIn
        );
        assertTrue(WBTC.balanceOf(ALICE) > 0);
    }

    function testDoubleEulerSwapLowBalance() public {
        // 10 USDC -(euler)-> RLUSD -(euler)-> USDT
        // We use RLUSD pools because it doesn't have significant balance on the pool
        // manager contract
        uint256 amountIn = 10_000000;
        deal(USDC_ADDR, address(uniswapV4Exposed), amountIn);
        address eulerProxy = 0xe0a80d35bB6618CBA260120b279d357978c42BCE;
        uint256 usdcEulerBalanceBefore = USDC.balanceOf(eulerProxy);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);

        // USDC -> RLUSD
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: RLUSD_ADDR,
            fee: uint24(40),
            tickSpacing: int24(1),
            hook: address(0x8B0DAD43EA6E83B2A6a0de18c5985030ba0Da8A8),
            hookData: bytes("")
        });
        // RLUSD -> USDT
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(50),
            tickSpacing: int24(1),
            hook: address(0xF87ACF8428F2f9403AAA0256A7272d6549ECa8A8),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDC_ADDR, USDT_ADDR, true, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertEq(USDC.balanceOf(eulerProxy), usdcEulerBalanceBefore + amountIn);
        assertTrue(USDT.balanceOf(ALICE) > 0);
    }

    function testMultipleSwapLastSwapEuler() public {
        // USDC -> RLUSD -(euler)- > USDT
        // Sanity check to see if a grouped swap with Euler in the last hop works
        uint256 amountIn = 134187695711754971245517404;
        deal(USDC_ADDR, address(uniswapV4Exposed), amountIn);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: RLUSD_ADDR,
            fee: uint24(500),
            tickSpacing: int24(10),
            hook: address(0),
            hookData: bytes("")
        });
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(50),
            tickSpacing: int24(1),
            hook: address(0xF87ACF8428F2f9403AAA0256A7272d6549ECa8A8),
            hookData: bytes("")
        });

        bytes memory data = UniswapV4Utils.encodeExactInput(
            USDC_ADDR, USDT_ADDR, false, false, pools
        );

        uniswapV4Exposed.swap(amountIn, data, ALICE);
        assertTrue(USDT.balanceOf(ALICE) > 0);
    }
}

contract TychoRouterForUniswapV4Test is TychoRouterTestSetup {
    function testSingleSwapUSV4CallbackPermit2() public {
        vm.startPrank(ALICE);
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, ALICE, amountIn);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(USDE_ADDR, tychoRouterAddr, amountIn);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory protocolData = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, USDT_ADDR, true, false, pools
        );

        bytes memory swap =
            encodeSingleSwap(address(usv4Executor), protocolData);

        tychoRouter.singleSwapPermit2(
            amountIn,
            USDE_ADDR,
            USDT_ADDR,
            99943850,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            swap
        );

        assertEq(IERC20(USDT_ADDR).balanceOf(ALICE), 99963618);
        vm.stopPrank();
    }

    function testSplitSwapMultipleUSV4Callback() public {
        // This test has two uniswap v4 hops that will be executed inside of the V4 pool manager
        // USDE -> USDT -> WBTC
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, ALICE, amountIn);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](2);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });
        pools[1] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WBTC_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory protocolData = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, WBTC_ADDR, true, false, pools
        );

        bytes memory swap =
            encodeSingleSwap(address(usv4Executor), protocolData);

        vm.startPrank(ALICE);
        IERC20(USDE_ADDR).approve(tychoRouterAddr, amountIn);
        tychoRouter.singleSwap(
            amountIn, USDE_ADDR, WBTC_ADDR, 118280, ALICE, noClientFee(), swap
        );

        assertEq(IERC20(WBTC_ADDR).balanceOf(ALICE), 118281);
    }

    function testSingleUSV4IntegrationGroupedSwap() public {
        // Test created with calldata from our router encoder.

        // Performs a single swap from USDC to PEPE though ETH using two
        // consecutive USV4 pools. It's a single swap because it is a consecutive grouped swaps
        //
        //   USDC ──(USV4)──> ETH ───(USV4)──> PEPE
        //
        deal(USDC_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(PEPE_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_usv4_grouped_swap"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(PEPE_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 123172000092711286554274694);
    }

    function testSingleUSV4IntegrationInputETH() public {
        // Test created with calldata from our router encoder.

        // Performs a single swap from ETH to PEPE without wrapping or unwrapping
        //
        //   ETH ───(USV4)──> PEPE
        //
        deal(ALICE, 1 ether);
        uint256 balanceBefore = IERC20(PEPE_ADDR).balanceOf(ALICE);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_usv4_eth_in");
        (bool success,) = tychoRouterAddr.call{value: 1 ether}(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(PEPE_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 235610487387677804636755778);
    }

    function testSingleUSV4IntegrationOutputETH() public {
        // Test created with calldata from our router encoder.

        // Performs a single swap from USDC to ETH without wrapping or unwrapping
        //
        //   USDC ───(USV4)──> ETH
        //
        deal(USDC_ADDR, ALICE, 3000_000000);
        uint256 balanceBefore = ALICE.balance;

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_usv4_eth_out");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = ALICE.balance;

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1474406268748155809);
    }
}

contract TychoRouterForUniswapV4AndEulerTest is TychoRouterTestSetup {
    function getForkBlock() public view virtual override returns (uint256) {
        return 22689128;
    }

    function testSingleUSV4AndHooksIntegrationGroupedSwap() public {
        // Test created with calldata from our router encoder.
        // Tests that uniswap_v4 and uniswap_v4_hooks can be grouped together
        //
        //   WETH ───(USV4 with Euler)──> USDC ──(USV4)──> ETH

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        uint256 balanceBefore = ALICE.balance;

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_usv4_and_hooks_grouped_swap"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = ALICE.balance;

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter - balanceBefore, 0.9 ether); // At least 0.9 ETH
    }
}

contract TychoRouterUSV4FeeTokenTest is TychoRouterTestSetup {
    function getForkBlock() public view virtual override returns (uint256) {
        return 23550000;
    }

    function testSwapTWIFToUSDCViaV4() public {
        // Full TychoRouter swap of TWIF (6% fee-on-transfer) to
        // USDC through a real UniswapV4 pool.
        //
        //   TWIF ───(USV4, fee=10000, tick=200)──> USDC
        //
        // TWIF charges 6% on every transfer, including to the
        // V4 Pool Manager. Before the fix, the executor passed
        // the full amountIn to V4's swap, but only 94% arrived
        // after the fee deduction, causing CurrencyNotSettled().
        address TWIF = 0x2Dd636C514Bb4705c756D161585Ff9ec665f18A2;

        // TWIF is nearly worthless (~7.6e-10 USDC per token).
        // Use a large amount so the swap produces >=1 USDC.
        uint256 amountIn = 1e34;

        deal(TWIF, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(TWIF).approve(PERMIT2_ADDRESS, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_usv4_twif_fee_token"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 usdcReceived = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "TychoRouter swap failed");
        assertGt(usdcReceived, 0, "Should receive USDC");
    }

    function testSwapUSDCToTWIFViaV4() public {
        // Full TychoRouter swap of USDC to TWIF (6% fee-on-transfer)
        // through a real UniswapV4 pool.
        //
        //   USDC ───(USV4, fee=10000, tick=200)──> TWIF
        //
        // TWIF charges 6% on the output transfer from the V4 Pool
        // Manager to the receiver. Before the fix, the executor
        // reported the pre-fee amount from the V4 delta, causing
        // TychoRouter__AmountOutNotFullyReceived(...)
        address TWIF = 0x2Dd636C514Bb4705c756D161585Ff9ec665f18A2;
        uint256 amountIn = 100_000000; // 100 USDC

        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_usv4_twif_fee_token_output"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 twifReceived = IERC20(TWIF).balanceOf(ALICE);

        assertTrue(success, "TychoRouter swap failed");
        assertGt(twifReceived, 0, "Should receive TWIF");
    }

    function testTwoHopTWIFIntermediaryViaV4() public {
        // UniswapV4 cyclical swaps encoded as two separate hops with TWIF (6%
        // fee-on-transfer) as intermediary. Same pool used in both directions to
        // isolate the effect of the transfer tax on the intermediate hop.
        //
        //   USDC ──(USV4)──> TWIF ──(USV4)──> USDC

        uint256 amountIn = 100_000000; // 100 USDC

        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_two_hop_usv4_twif_intermediary");
        (bool success, bytes memory returnData) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 usdcReceived = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 reportedAmountOut = abi.decode(returnData, (uint256));

        assertTrue(success, "TychoRouter grouped TWIF swap failed");
        assertGt(usdcReceived, 0, "Should receive USDC back");
        assertEq(
            reportedAmountOut,
            usdcReceived,
            "Reported amountOut should match actual balance received"
        );
    }
}

struct Loan {
    address token;
    uint256 amount;
}

/// @title Example filler that takes PM loans and routes swaps
///   through TychoRouter with skipUnlock=true.
contract ExternalSettlerMock {
    using SafeERC20 for IERC20;

    IPoolManager public immutable poolManager;
    address public immutable tychoRouter;

    constructor(IPoolManager poolManager_, address tychoRouter_) {
        poolManager = poolManager_;
        tychoRouter = tychoRouter_;
    }

    function settle(
        Loan[] calldata loans,
        address tokenIn,
        address v4SyncToken,
        uint256 amountIn,
        bytes calldata tychoCalldata
    ) external {
        IERC20(tokenIn).forceApprove(tychoRouter, amountIn);
        // slither-disable-next-line unused-return
        poolManager.unlock(abi.encode(loans, v4SyncToken, tychoCalldata));
    }

    function unlockCallback(bytes calldata data)
        external
        returns (bytes memory)
    {
        require(msg.sender == address(poolManager));
        (Loan[] memory loans, address v4SyncToken, bytes memory tychoCalldata) =
            abi.decode(data, (Loan[], address, bytes));

        // 1. Take loans
        for (uint256 i = 0; i < loans.length; i++) {
            poolManager.take(
                Currency.wrap(loans[i].token), address(this), loans[i].amount
            );
        }

        // 2. Sync the V4 hop input token (required for skipUnlock=true).
        //    In a sequential route this is the V4 input, which may differ
        //    from the router's input token.
        poolManager.sync(Currency.wrap(v4SyncToken));

        // 3. Route through TychoRouter (replaces the old
        //    direct executor call + safeTransfer pattern)
        // slither-disable-next-line low-level-calls
        (bool success, bytes memory result) = tychoRouter.call(tychoCalldata);
        if (!success) {
            if (result.length == 0) revert("Fill failed");
            assembly {
                revert(add(result, 0x20), mload(result))
            }
        }

        // 4. Repay loans
        for (uint256 i = 0; i < loans.length; i++) {
            Currency currency = Currency.wrap(loans[i].token);
            poolManager.sync(currency);
            IERC20(loans[i].token)
                .safeTransfer(address(poolManager), loans[i].amount);
            // slither-disable-next-line unused-return
            poolManager.settle();
        }

        return result;
    }
}

contract ExternalSettlerNoUnlockTest is TychoRouterTestSetup {
    ExternalSettlerMock settler;

    function getForkBlock() public view virtual override returns (uint256) {
        return 22689128;
    }

    function setUp() public override {
        super.setUp();
        settler = new ExternalSettlerMock(
            IPoolManager(POOL_MANAGER), tychoRouterAddr
        );
    }

    function testSettlementWithLoanSingleSwap() public {
        // External settler with loan + skipUnlock:
        //   1. Take USDE loan from PM
        //   2. Sync USDE (required for skipUnlock)
        //   3. Swap loaned USDE → USDT via TychoRouter
        //      (Dispatcher transfers from settler to PM)
        //   4. Repay USDE loan from settler's own capital

        uint256 swapAmount = 100 ether;
        uint256 loanAmount = 100 ether;

        // Settler needs own capital to repay the loan
        deal(USDE_ADDR, address(settler), loanAmount);

        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });

        bytes memory protocolData = UniswapV4Utils.encodeExactInput(
            USDE_ADDR, USDT_ADDR, true, true, pools
        );
        bytes memory swap =
            encodeSingleSwap(address(usv4Executor), protocolData);

        bytes memory tychoCalldata = abi.encodeCall(
            tychoRouter.singleSwap,
            (
                swapAmount,
                USDE_ADDR,
                USDT_ADDR,
                1,
                address(settler),
                noClientFee(),
                swap
            )
        );

        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: USDE_ADDR, amount: loanAmount});

        settler.settle(loans, USDE_ADDR, USDE_ADDR, swapAmount, tychoCalldata);

        assertEq(
            IERC20(USDT_ADDR).balanceOf(address(settler)),
            99970662,
            "Settler should receive USDT"
        );
        assertEq(
            IERC20(USDE_ADDR).balanceOf(address(settler)),
            0,
            "Settler USDE used for loan repayment"
        );
    }

    function testSettlementWithLoanSequentialV2ThenV4() public {
        //        WETH ──(UniV2)──> WBTC ──(UniV4 skipUnlock)──> USDT
        // External settler with loan + skipUnlock on a sequential route:
        //   1. Take WETH loan from PM
        //   2. Sync WBTC — the V4 hop's input, deposited into PM by the V2 hop
        //   3. Sequential swap loaned WETH → USDT via TychoRouter
        //   4. Repay WETH loan from settler's own capital

        uint256 swapAmount = 1 ether;
        uint256 loanAmount = 1 ether;

        // Settler needs own capital to repay the loan
        deal(WETH_ADDR, address(settler), loanAmount);

        bytes[] memory swaps = new bytes[](2);

        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );

        UniswapV4Executor.UniswapV4Pool[] memory v4Pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4Pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });
        bytes memory v4ProtocolData = UniswapV4Utils.encodeExactInput(
            WBTC_ADDR, USDT_ADDR, true, true, v4Pools
        );
        swaps[1] = encodeSequentialSwap(address(usv4Executor), v4ProtocolData);

        bytes memory tychoCalldata = abi.encodeCall(
            tychoRouter.sequentialSwap,
            (
                swapAmount,
                WETH_ADDR,
                USDT_ADDR,
                1,
                address(settler),
                noClientFee(),
                pleEncode(swaps)
            )
        );

        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: WETH_ADDR, amount: loanAmount});

        settler.settle(loans, WETH_ADDR, WBTC_ADDR, swapAmount, tychoCalldata);

        assertEq(
            IERC20(USDT_ADDR).balanceOf(address(settler)),
            2734715627,
            "Settler should receive USDT"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(settler)),
            0,
            "Settler WETH used for loan repayment"
        );
    }

    function testSettlementWithLoanSequentialV4ThenV2() public {
        //   USDT ──(UniV4 skipUnlock)──> WBTC ──(UniV2)──> WETH
        // The V4 hop's input == router input == loan token, so the existing
        // sync(tokenIn) on the settler is correct (no extra sync needed).

        uint256 swapAmount = 100 * 1e6; // 100 USDT
        uint256 loanAmount = 100 * 1e6;

        // Settler needs own capital to repay the loan
        deal(USDT_ADDR, address(settler), loanAmount);

        bytes[] memory swaps = new bytes[](2);

        UniswapV4Executor.UniswapV4Pool[] memory v4Pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4Pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WBTC_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });
        bytes memory v4ProtocolData = UniswapV4Utils.encodeExactInput(
            USDT_ADDR, WBTC_ADDR, false, true, v4Pools
        );
        swaps[0] = encodeSequentialSwap(address(usv4Executor), v4ProtocolData);

        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WBTC_ADDR, WETH_ADDR)
        );

        bytes memory tychoCalldata = abi.encodeCall(
            tychoRouter.sequentialSwap,
            (
                swapAmount,
                USDT_ADDR,
                WETH_ADDR,
                1,
                address(settler),
                noClientFee(),
                pleEncode(swaps)
            )
        );

        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: USDT_ADDR, amount: loanAmount});

        settler.settle(loans, USDT_ADDR, USDT_ADDR, swapAmount, tychoCalldata);

        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(settler)),
            36114721806078051,
            "Settler should receive WETH"
        );
        assertEq(
            IERC20(USDT_ADDR).balanceOf(address(settler)),
            0,
            "Settler USDT used for loan repayment"
        );
    }

    function testSettlementWithLoanSequentialV4ThenV4() public {
        //   USDE ──(UniV4 skipUnlock)──> USDT ──(UniV4 skipUnlock)──> WBTC
        // Hop 1's end-of-swap sync(tokenOut)=sync(USDT) primes the PM for
        // hop 2's settle(USDT), so the settler only needs to sync USDE.

        uint256 swapAmount = 100 ether;
        uint256 loanAmount = 100 ether;

        // Settler needs own capital to repay the loan
        deal(USDE_ADDR, address(settler), loanAmount);

        bytes[] memory swaps = new bytes[](2);

        UniswapV4Executor.UniswapV4Pool[] memory v4PoolsHop1 =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4PoolsHop1[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });
        swaps[0] = encodeSequentialSwap(
            address(usv4Executor),
            UniswapV4Utils.encodeExactInput(
                USDE_ADDR, USDT_ADDR, true, true, v4PoolsHop1
            )
        );

        UniswapV4Executor.UniswapV4Pool[] memory v4PoolsHop2 =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4PoolsHop2[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: WBTC_ADDR,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });
        swaps[1] = encodeSequentialSwap(
            address(usv4Executor),
            UniswapV4Utils.encodeExactInput(
                USDT_ADDR, WBTC_ADDR, false, true, v4PoolsHop2
            )
        );

        bytes memory tychoCalldata = abi.encodeCall(
            tychoRouter.sequentialSwap,
            (
                swapAmount,
                USDE_ADDR,
                WBTC_ADDR,
                1,
                address(settler),
                noClientFee(),
                pleEncode(swaps)
            )
        );

        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: USDE_ADDR, amount: loanAmount});

        settler.settle(loans, USDE_ADDR, USDE_ADDR, swapAmount, tychoCalldata);

        assertEq(
            IERC20(WBTC_ADDR).balanceOf(address(settler)),
            92926,
            "Settler should receive WBTC"
        );
        assertEq(
            IERC20(USDE_ADDR).balanceOf(address(settler)),
            0,
            "Settler USDE used for loan repayment"
        );
    }

    function testSettlementWithLoanSequentialV4ThenV3ThenV4() public {
        //   USDE ─(UniV4 skipUnlock)─> USDT ─(UniV3)─> DAI ─(UniV4 skipUnlock)─> USDC
        // Documents the limitation of the current UniswapV4Executor.sol.
        // Hop 1's end-of-swap sync(tokenOut)=sync(USDT) primes the
        // PM only for a following V4 hop on USDT. The V3 hop in between
        // deposits DAI into the PM without re-syncing, so by the time hop 3
        // calls _settle the PM's synced currency is still USDT — the credit
        // for DAI is zero and V4's swap attempts amountSpecified=0 and
        // reverts. Wrapped in expectRevert until the sync-chaining issue
        // is addressed in the V4 executor (future work).

        uint256 swapAmount = 100 ether;
        uint256 loanAmount = 100 ether;
        deal(USDE_ADDR, address(settler), loanAmount);

        bytes[] memory swaps = new bytes[](3);

        UniswapV4Executor.UniswapV4Pool[] memory v4PoolsHop1 =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4PoolsHop1[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDT_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });
        swaps[0] = encodeSequentialSwap(
            address(usv4Executor),
            UniswapV4Utils.encodeExactInput(
                USDE_ADDR, USDT_ADDR, true, true, v4PoolsHop1
            )
        );

        swaps[1] = encodeSequentialSwap(
            address(usv3Executor),
            encodeUniswapV3Swap(USDT_ADDR, DAI_ADDR, DAI_USDT_USV3, false)
        );

        UniswapV4Executor.UniswapV4Pool[] memory v4PoolsHop3 =
            new UniswapV4Executor.UniswapV4Pool[](1);
        v4PoolsHop3[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: USDC_ADDR,
            fee: uint24(100),
            tickSpacing: int24(1),
            hook: address(0),
            hookData: bytes("")
        });
        swaps[2] = encodeSequentialSwap(
            address(usv4Executor),
            UniswapV4Utils.encodeExactInput(
                DAI_ADDR, USDC_ADDR, true, true, v4PoolsHop3
            )
        );

        bytes memory tychoCalldata = abi.encodeCall(
            tychoRouter.sequentialSwap,
            (
                swapAmount,
                USDE_ADDR,
                USDC_ADDR,
                1,
                address(settler),
                noClientFee(),
                pleEncode(swaps)
            )
        );

        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: USDE_ADDR, amount: loanAmount});

        vm.expectRevert(IPoolManager.SwapAmountCannotBeZero.selector);
        settler.settle(loans, USDE_ADDR, USDE_ADDR, swapAmount, tychoCalldata);
    }
}

