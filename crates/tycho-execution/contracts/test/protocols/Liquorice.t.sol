// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/LiquoriceExecutor.sol";
import {Constants} from "../Constants.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

interface ILiquoriceSettlement {
    function BALANCE_MANAGER() external view returns (address);

    function AUTHENTICATOR() external view returns (address);
}

interface IAllowListAuthentication {
    function addSolver(address _solver) external;

    function addMaker(address _maker) external;
}

contract LiquoriceExecutorExposed is LiquoriceExecutor {
    constructor(address _liquoriceSettlement, address _liquoriceBalanceManager)
        LiquoriceExecutor(_liquoriceSettlement, _liquoriceBalanceManager)
    {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            uint32 partialFillOffset,
            uint256 originalBaseTokenAmount,
            uint256 minBaseTokenAmount,
            bytes memory liquoriceCalldata
        )
    {
        return _decodeData(data);
    }

    function clampAmount(
        uint256 givenAmount,
        uint256 originalBaseTokenAmount,
        uint256 minBaseTokenAmount
    ) external pure returns (uint256) {
        return _clampAmount(
            givenAmount, originalBaseTokenAmount, minBaseTokenAmount
        );
    }
}

contract LiquoriceExecutorTest is Constants, Permit2TestHelper, TestUtils {
    using SafeERC20 for IERC20;

    ILiquoriceSettlement liquoriceSettlement;
    IAllowListAuthentication authenticator;
    LiquoriceExecutorExposed liquoriceExecutor;

    address constant AUTH_MANAGER = 0x000438801500c89E225E8D6CB69D9c14dD05e000;

    address constant MAKER = 0x06465bcEEaef280Bb7340A58D75dfc5E1F687058;
    uint256 constant FORK_BLOCK = 24_392_845;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        liquoriceSettlement = ILiquoriceSettlement(LIQUORICE_SETTLEMENT);

        liquoriceExecutor = new LiquoriceExecutorExposed(
            LIQUORICE_SETTLEMENT, LIQUORICE_BALANCE_MANAGER
        );
        authenticator =
            IAllowListAuthentication(liquoriceSettlement.AUTHENTICATOR());

        vm.prank(AUTH_MANAGER);
        authenticator.addSolver(address(liquoriceExecutor));
        vm.prank(AUTH_MANAGER);
        authenticator.addMaker(MAKER);

        vm.prank(MAKER);
        IERC20(WETH_ADDR).approve(LIQUORICE_BALANCE_MANAGER, type(uint256).max);
    }

    function testSettleSingle() public {
        // 3000 USDC -> 1 WETH
        bytes memory liquoriceCalldata =
            hex"9935c86800000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f68705800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000000000000000320000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058000000000000000000000000000000000000000000000000000000000000000131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000041883a6506193307eebda0f3adf2cb81f84a073e030749055ebb18cbf98704eef100a03c307266527d706f9a5c3e08ed0988f5b130bc5327e0ad62dde6f3709d251b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";

        address tokenIn = USDC_ADDR;
        address tokenOut = WETH_ADDR;
        uint32 partialFillOffset = 96;
        uint256 amountIn = 3000e6;
        uint256 expectedAmountOut = 1 ether;

        deal(tokenOut, MAKER, expectedAmountOut);
        deal(tokenIn, address(liquoriceExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            partialFillOffset,
            amountIn, // originalBaseTokenAmount
            amountIn, // minBaseTokenAmount (same for full fill)
            liquoriceCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor));
        vm.prank(address(liquoriceExecutor));
        IERC20(tokenIn).approve(LIQUORICE_BALANCE_MANAGER, amountIn);

        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "WETH should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(liquoriceExecutor)),
            0,
            "USDC left in executor"
        );
    }

    function testSettleSingle_PartialFill() public {
        bytes memory liquoriceCalldata =
            hex"9935c86800000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f68705800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000000000000000320000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058000000000000000000000000000000000000000000000000000000000000000131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000041883a6506193307eebda0f3adf2cb81f84a073e030749055ebb18cbf98704eef100a03c307266527d706f9a5c3e08ed0988f5b130bc5327e0ad62dde6f3709d251b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";

        address tokenIn = USDC_ADDR;
        address tokenOut = WETH_ADDR;
        uint32 partialFillOffset = 96;
        uint256 originalAmountIn = 3000e6;
        uint256 amountIn = 1500e6;
        uint256 minAmountIn = 1500e6;
        uint256 expectedAmountOut = 0.5 ether;

        deal(tokenOut, MAKER, expectedAmountOut);
        deal(tokenIn, address(liquoriceExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            partialFillOffset,
            originalAmountIn,
            minAmountIn,
            liquoriceCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor));
        vm.prank(address(liquoriceExecutor));
        IERC20(tokenIn).approve(LIQUORICE_BALANCE_MANAGER, amountIn);

        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "WETH should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(liquoriceExecutor)),
            0,
            "USDC left in executor"
        );
    }

    function testSettle() public {
        bytes memory liquoriceCalldata =
            hex"cba673a700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f68705800000000000000000000000000000000000000000000000000000000b2d05e0000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000038000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000042000000000000000000000000000000000000000000000000000000000000005000000000000000000000000000448633eb8b0a42efed924c42069e0dcf08fb5520000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000026000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f6870580000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000b2d05e0000000000000000000000000000000000000000000000000000000000b2d05e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000de0b6b3a764000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000411d85c337d0e071eb601d8a90e2e8dd0afb61db200a1614c4afe5d26ff0c11bd402e018ab5fdbf8386437d7594af4383cf020a75c96e02c0a208f0b06e86115401b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";

        address tokenIn = USDC_ADDR;
        address tokenOut = WETH_ADDR;
        uint32 partialFillOffset = 32;
        uint256 amountIn = 3000e6;
        uint256 expectedAmountOut = 1 ether;

        deal(tokenOut, MAKER, expectedAmountOut);
        deal(tokenIn, address(liquoriceExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            partialFillOffset,
            amountIn,
            amountIn,
            liquoriceCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor));
        vm.prank(address(liquoriceExecutor));
        IERC20(tokenIn).approve(LIQUORICE_BALANCE_MANAGER, amountIn);

        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "WETH should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(liquoriceExecutor)),
            0,
            "USDC left in executor"
        );
    }

    function testSettle_PartialFill() public {
        bytes memory liquoriceCalldata =
            hex"cba673a700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000038000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000042000000000000000000000000000000000000000000000000000000000000005000000000000000000000000000448633eb8b0a42efed924c42069e0dcf08fb5520000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000026000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f6870580000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000b2d05e0000000000000000000000000000000000000000000000000000000000b2d05e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000013100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000041e89ad636a6d749213b9339ac5218229adaa53bdf96d457ee2cebfd4fd02909bf678953c976ceca7307ef2e73c5687c33738a6a1e4130e378d220b68d9c59e18b1b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";

        address tokenIn = USDC_ADDR;
        address tokenOut = WETH_ADDR;
        uint32 partialFillOffset = 32;
        uint256 originalAmountIn = 3000e6;
        uint256 amountIn = 1500e6;
        uint256 minAmountIn = 1500e6;
        uint256 expectedAmountOut = 0.5 ether;

        deal(tokenOut, MAKER, expectedAmountOut);
        deal(tokenIn, address(liquoriceExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            partialFillOffset,
            originalAmountIn,
            minAmountIn,
            liquoriceCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor));
        vm.prank(address(liquoriceExecutor));
        IERC20(tokenIn).approve(LIQUORICE_BALANCE_MANAGER, amountIn);

        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(liquoriceExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "WETH should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(liquoriceExecutor)),
            0,
            "USDC left in executor"
        );
    }

    function testDecodeData() public view {
        bytes memory liquoriceCalldata = abi.encodePacked(
            bytes4(0xdeadbeef),
            hex"1234567890abcdef1234567890abcdef"
            hex"1234567890abcdef1234567890abcdef"
        );

        uint256 originalAmount = 1000000000;
        uint256 minAmount = 800000000;

        bytes memory params = abi.encodePacked(
            USDC_ADDR, // tokenIn (20 bytes)
            WETH_ADDR, // tokenOut (20 bytes)
            uint32(5), // partialFillOffset (4 bytes)
            originalAmount, // originalBaseTokenAmount (32 bytes)
            minAmount, // minBaseTokenAmount (32 bytes)
            liquoriceCalldata // variable length
        );

        (
            address tokenIn,
            uint32 decodedPartialFillOffset,
            uint256 decodedOriginalAmount,
            uint256 decodedMinAmount,
            bytes memory decodedCalldata
        ) = liquoriceExecutor.decodeData(params);

        assertEq(tokenIn, USDC_ADDR);
        assertEq(decodedPartialFillOffset, 5, "partialFillOffset mismatch");
        assertEq(
            decodedOriginalAmount, originalAmount, "originalAmount mismatch"
        );
        assertEq(decodedMinAmount, minAmount, "minAmount mismatch");
        assertEq(
            keccak256(decodedCalldata),
            keccak256(liquoriceCalldata),
            "calldata mismatch"
        );
    }

    function testDecodeData_InvalidDataLength() public {
        bytes memory tooShort =
            abi.encodePacked(USDC_ADDR, WETH_ADDR, uint32(0));

        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__InvalidDataLength.selector
        );
        liquoriceExecutor.decodeData(tooShort);
    }

    function testInvalidSelector() public {
        bytes memory badCalldata = abi.encodePacked(
            bytes4(0xdeadbeef),
            hex"0000000000000000000000000000000000000000000000000000000000000000"
        );

        uint256 amountIn = 1000e6;
        bytes memory params = abi.encodePacked(
            USDC_ADDR, WETH_ADDR, uint32(0), amountIn, amountIn, badCalldata
        );

        deal(USDC_ADDR, address(liquoriceExecutor), amountIn);

        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__InvalidSelector.selector
        );
        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));
    }

    function testConstructor_NotAContract_Settlement() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__NotAContract.selector
        );
        new LiquoriceExecutorExposed(address(0x1), LIQUORICE_BALANCE_MANAGER);
    }

    function testConstructor_NotAContract_BalanceManager() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__NotAContract.selector
        );
        new LiquoriceExecutorExposed(LIQUORICE_SETTLEMENT, address(0x1));
    }

    function testClampAmount_WithinRange() public view {
        uint256 result = liquoriceExecutor.clampAmount(500, 1000, 100);
        assertEq(result, 500, "Should return givenAmount when within range");
    }

    function testClampAmount_ExceedsMax() public view {
        uint256 result = liquoriceExecutor.clampAmount(1500, 1000, 100);
        assertEq(
            result,
            1000,
            "Should clamp to originalBaseTokenAmount when exceeded"
        );
    }

    function testClampAmount_BelowMin_Reverts() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__AmountBelowMinimum.selector
        );
        liquoriceExecutor.clampAmount(50, 1000, 100);
    }
}

contract TychoRouterForLiquoriceTest is TychoRouterTestSetup {
    using SafeERC20 for IERC20;

    address constant AUTH_MANAGER = 0x000438801500c89E225E8D6CB69D9c14dD05e000;
    address constant MAKER = 0x06465bcEEaef280Bb7340A58D75dfc5E1F687058;

    function getForkBlock() public pure override returns (uint256) {
        return 24392845;
    }

    function setUp() public override {
        super.setUp();

        ILiquoriceSettlement settlement =
            ILiquoriceSettlement(LIQUORICE_SETTLEMENT);
        IAllowListAuthentication authenticator =
            IAllowListAuthentication(settlement.AUTHENTICATOR());

        vm.prank(AUTH_MANAGER);
        authenticator.addSolver(address(tychoRouter));
        vm.prank(AUTH_MANAGER);
        authenticator.addMaker(MAKER);

        vm.prank(MAKER);
        IERC20(WETH_ADDR).approve(LIQUORICE_BALANCE_MANAGER, type(uint256).max);
    }

    function testSettleSingleLiquoriceIntegration() public {
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 3000e6);
        deal(WETH_ADDR, MAKER, 1 ether);
        uint256 expAmountOut = 1 ether;

        uint256 wethBefore = IERC20(WETH_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_liquorice_settle_single"
        );
        // Mock ecrecover precompile to return MAKER, bypassing order signature
        // verification. The test calldata uses a modified user address, so the
        // original signature no longer recovers to the correct maker.
        vm.mockCall(address(0x01), abi.encode(), abi.encode(MAKER));

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        uint256 wethReceived = IERC20(WETH_ADDR).balanceOf(user) - wethBefore;
        assertEq(wethReceived, expAmountOut, "Incorrect WETH received");
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "USDC left in router"
        );
        vm.stopPrank();
    }

    function testSettleLiquoriceIntegration() public {
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 3000e6);
        deal(WETH_ADDR, MAKER, 1 ether);
        uint256 expAmountOut = 1 ether;

        uint256 wethBefore = IERC20(WETH_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_liquorice_settle"
        );
        // Mock ecrecover precompile to return MAKER, bypassing order signature
        // verification. The test calldata uses a modified user address, so the
        // original signature no longer recovers to the correct maker.
        vm.mockCall(address(0x01), abi.encode(), abi.encode(MAKER));

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        uint256 wethReceived = IERC20(WETH_ADDR).balanceOf(user) - wethBefore;
        assertEq(wethReceived, expAmountOut, "Incorrect WETH received");
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "USDC left in router"
        );
        vm.stopPrank();
    }
}
