pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/ERC4626Executor.sol";
import {ALICE, Constants, WETH_ADDR} from "../Constants.sol";
import {ExecutorDeployment} from "../ExecutorDeployment.sol";
import {ERC4626Deployment} from "../deployments/ERC4626.sol";

contract ERC4626ExecutorExposed is ERC4626Executor {
    constructor() ERC4626Executor() {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (IERC20 inToken, address target)
    {
        return _decodeData(data);
    }
}

/// Malicious vault that takes input tokens but returns a false amount without
/// minting anything.
contract FakeERC4626Vault is Constants {
    function asset() external view returns (address) {
        return WETH_ADDR;
    }

    function balanceOf(address) external pure returns (uint256) {
        return 0;
    }

    function deposit(uint256 assets, address) external returns (uint256) {
        IERC20(WETH_ADDR).transferFrom(msg.sender, address(this), assets);
        // Lie: claim 999e18 shares minted, but send nothing
        return 999e18;
    }
}

contract ERC4626ExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    ERC4626ExecutorExposed ERC4626Exposed;
    IERC20 WETH = IERC20(WETH_ADDR);
    IERC4626 spETH = IERC4626(0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f);

    function setUp() public {
        uint256 forkBlock = 23922291;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        ERC4626Exposed = new ERC4626ExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(WETH_ADDR, address(spETH));

        (IERC20 inToken, address target) = ERC4626Exposed.decodeParams(params);

        assertEq(address(inToken), WETH_ADDR);
        assertEq(address(target), address(spETH));
    }

    function testDecodeParamsInvalidDataLength() public {
        // Pass 61 bytes (one extra) to trigger invalid length error
        bytes memory invalidParams =
            abi.encodePacked(WETH_ADDR, address(spETH), uint8(0));

        vm.expectRevert(ERC4626Executor__InvalidDataLength.selector);
        ERC4626Exposed.decodeParams(invalidParams);
    }

    function testGetTransferData() public {
        bytes memory params = abi.encodePacked(WETH_ADDR, address(spETH));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = ERC4626Exposed.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, address(spETH));
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, address(spETH));
        assertEq(outputToRouter, false);
    }

    function testDeposit() public {
        uint256 amountIn = 10 ** 18;
        bytes memory protocolData = abi.encodePacked(WETH_ADDR, address(spETH));

        deal(WETH_ADDR, address(ERC4626Exposed), amountIn);

        uint256 balanceBefore = spETH.balanceOf(BOB);

        vm.prank(address(ERC4626Exposed));
        IERC20(WETH_ADDR).approve(address(spETH), amountIn);
        ERC4626Exposed.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter = spETH.balanceOf(BOB);
        assertGt(balanceAfter, balanceBefore);
    }

    function testRedeem() public {
        uint256 amountIn = 10 ** 18;
        bytes memory protocolData =
            abi.encodePacked(address(spETH), address(spETH));

        deal(address(spETH), address(ERC4626Exposed), amountIn);

        uint256 balanceBefore = WETH.balanceOf(BOB);

        ERC4626Exposed.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter = WETH.balanceOf(BOB);
        assertGt(balanceAfter, balanceBefore);
    }

    function testFakeVault() public {
        FakeERC4626Vault fakeVault = new FakeERC4626Vault();
        uint256 amountIn = 1e18;
        deal(WETH_ADDR, address(ERC4626Exposed), amountIn);

        vm.prank(address(ERC4626Exposed));
        IERC20(WETH_ADDR).approve(address(fakeVault), amountIn);

        bytes memory protocolData =
            abi.encodePacked(WETH_ADDR, address(fakeVault));

        uint256 balanceBefore = IERC20(address(fakeVault)).balanceOf(BOB);
        ERC4626Exposed.swap(amountIn, protocolData, BOB);
        uint256 balanceAfter = IERC20(address(fakeVault)).balanceOf(BOB);

        // Balance check produces 0 — fake vault sent nothing
        assertEq(balanceAfter - balanceBefore, 0);
    }
}

contract TychoRouterForERC4626Test is TychoRouterTestSetup, ERC4626Deployment {
    function getForkBlock() public pure override returns (uint256) {
        return 23889103;
    }

    function testSingleERC4626Integration() public {
        IERC4626 spETH = IERC4626(0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f);
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = spETH.balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_erc4626");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = spETH.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }

    function testSequentialERC4626Integration() public {
        // spUSDC -> (ERC4626) -> USDC -> (ERC4626) -> sUSDC
        IERC4626 spusdc = IERC4626(0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d);
        IERC4626 susdc = IERC4626(0xBc65ad17c5C0a2A4D159fa5a503f4992c7B545FE);
        deal(address(spusdc), ALICE, 100e6);
        uint256 balanceBefore = susdc.balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(address(spusdc)).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_sequential_encoding_strategy_erc4626");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = susdc.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(spusdc.balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }
}
