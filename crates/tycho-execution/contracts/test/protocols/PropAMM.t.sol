pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    PropAMMExecutor,
    PropAMMExecutor__InvalidDataLength
} from "../../src/executors/PropAMMExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {MockERC20} from "forge-std/mocks/MockERC20.sol";

/// @notice Minimal `IPropAMM` implementation paying a configurable fixed
/// price per pair out of its own (dealt) balance, honoring the push-payment
/// model the executor relies on.
contract MockPropAMM is IPropAMM {
    using SafeERC20 for IERC20;

    /// amountOut per 1e18 units of tokenIn, per (tokenIn, tokenOut).
    mapping(address => mapping(address => uint256)) public priceWad;

    function setPrice(address tokenIn, address tokenOut, uint256 priceWad_)
        external
    {
        priceWad[tokenIn][tokenOut] = priceWad_;
    }

    function isActive(address tokenIn, address tokenOut)
        external
        view
        returns (bool active)
    {
        return priceWad[tokenIn][tokenOut] != 0;
    }

    function getPairs() external pure returns (TokenPair[] memory pairs) {
        return new TokenPair[](0);
    }

    function quote(address tokenIn, address tokenOut, uint256 amountIn)
        public
        view
        returns (uint256 amountOut)
    {
        uint256 price = priceWad[tokenIn][tokenOut];
        require(price != 0, "MockPropAMM: inactive pair");
        return amountIn * price / 1e18;
    }

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external returns (uint256 amountOut) {
        require(deadline >= block.timestamp, "MockPropAMM: expired");
        // Push-payment model: the caller must have transferred amountIn of
        // tokenIn to this contract already.
        require(
            IERC20(tokenIn).balanceOf(address(this)) >= amountIn,
            "MockPropAMM: not funded"
        );
        amountOut = quote(tokenIn, tokenOut, amountIn);
        require(amountOut >= minAmountOut, "MockPropAMM: insufficient output");
        IERC20(tokenOut).safeTransfer(recipient, amountOut);
        emit Swapped(
            msg.sender, tokenIn, tokenOut, amountIn, amountOut, recipient
        );
    }
}

contract PropAMMExecutorExposed is PropAMMExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address pamm, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract PropAMMExecutorTest is TestUtils, Constants {
    // Must match the pAMM address used by the Rust encoding tests that
    // generate this suite's calldata.
    address constant MOCK_PAMM = 0x1111111111111111111111111111111111111111;
    // 2000 USDC (6 decimals) per 1e18 units (1 WETH) of tokenIn.
    uint256 constant WETH_USDC_PRICE = 2000e6;

    PropAMMExecutorExposed executor;

    function setUp() public {
        // Everything this suite touches is mocked, so no fork is needed: mock
        // ERC20s are etched at the canonical token addresses embedded in the
        // Rust-generated calldata.
        executor = new PropAMMExecutorExposed();
        deployCodeTo("MockERC20.sol:MockERC20", WETH_ADDR);
        MockERC20(WETH_ADDR).initialize("Wrapped Ether", "WETH", 18);
        deployCodeTo("MockERC20.sol:MockERC20", USDC_ADDR);
        MockERC20(USDC_ADDR).initialize("USD Coin", "USDC", 6);
        deployCodeTo("PropAMM.t.sol:MockPropAMM", MOCK_PAMM);
        MockPropAMM(MOCK_PAMM).setPrice(WETH_ADDR, USDC_ADDR, WETH_USDC_PRICE);
    }

    function testDecodeParams() public view {
        (address pamm, address tokenIn, address tokenOut) =
            executor.decodeParams(_params());

        assertEq(pamm, MOCK_PAMM);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(PropAMMExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(MOCK_PAMM, WETH_ADDR));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_params());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, MOCK_PAMM);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(executor.fundsExpectedAddress(_params()), MOCK_PAMM);
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 1 ether;

        // The router transfers the input to the pAMM before calling the
        // executor (push-payment); the deal below simulates that transfer and
        // funds the mock's output inventory.
        deal(WETH_ADDR, MOCK_PAMM, amountIn);
        deal(USDC_ADDR, MOCK_PAMM, 1_000_000e6);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        executor.swap(amountIn, _params(), BOB);
        uint256 usdcDelta = IERC20(USDC_ADDR).balanceOf(BOB) - usdcBalanceBefore;

        assertEq(usdcDelta, 2000e6);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_propamm_weth_usdc");

        (address pamm, address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(pamm, MOCK_PAMM);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function _params() internal view returns (bytes memory) {
        return abi.encodePacked(MOCK_PAMM, WETH_ADDR, USDC_ADDR);
    }
}

contract PropAMMRouterTest is TychoRouterTestSetup {
    address constant MOCK_PAMM = 0x1111111111111111111111111111111111111111;

    function getForkBlock() public pure override returns (uint256) {
        return 25143884;
    }

    function setUp() public override {
        super.setUp();
        deployCodeTo("PropAMM.t.sol:MockPropAMM", MOCK_PAMM);
        MockPropAMM(MOCK_PAMM).setPrice(WETH_ADDR, USDC_ADDR, 2000e6);
        deal(USDC_ADDR, MOCK_PAMM, 1_000_000e6);
    }

    function testSingleSwap() public {
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_propamm_weth_usdc"
        );

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 wethBalanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 usdcDelta =
            IERC20(USDC_ADDR).balanceOf(ALICE) - usdcBalanceBefore;
        uint256 wethDelta =
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(usdcDelta, 2000e6);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
