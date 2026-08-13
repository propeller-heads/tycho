pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    TempestExecutor,
    TempestExecutor__InvalidDataLength,
    TempestExecutor__ZeroRouterAddress
} from "../../src/executors/TempestExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";

/// @dev The fork block is the maker's last committed USDC/WETH lane. Lane
/// payloads persist in registry storage after the commit, so the ladder is
/// readable; only the timestamp goes stale, which `_refreshLane` restamps.
uint256 constant TEMPEST_FORK_BLOCK = 25673715;

/// @dev Shared fork fixtures for Tempest: a swap only settles if the taker is
/// allowlisted and the lane is inside the router's freshness window. On chain
/// the builder guarantees the latter by ordering the maker's quote tx directly
/// ahead of the fill; here both are set with `vm.store`.
abstract contract TempestFixtures is Constants {
    /// `Tempest.allowedTaker` is the second declared slot (OZ v5 bases use
    /// ERC-7201 namespaced storage, so the contract's own vars start at 0:
    /// `oracle` 0, `allowedTaker` 1, `vault` 2, `_pairs` 3, `pairRegistered` 4,
    /// `flashTaker` 5).
    ///
    /// This and the lane bit layout below are read straight from the deployed
    /// implementation's storage. The router is an upgradeable proxy, so a future
    /// `upgradeToAndCall` that reorders slots or changes the lane packing will
    /// break these fixtures. Failures are loud rather than silent -- the
    /// `storedSlotCount > 0` guard rejects a fork block with no committed
    /// ladder, and a wrong taker slot reverts `TakerNotAllowed`.
    uint256 private constant ALLOWED_TAKER_SLOT = 1;

    function _allowTaker(address taker) internal {
        vm.store(
            TEMPEST_ROUTER,
            keccak256(abi.encode(taker, ALLOWED_TAKER_SLOT)),
            bytes32(uint256(1))
        );
    }

    function _refreshLane(address tokenA, address tokenB) internal {
        bytes32 laneSlot = keccak256(
            abi.encode(TEMPEST_ROUTER, uint256(_lane(tokenA, tokenB)))
        );
        uint256 storedLane = uint256(vm.load(TEMPEST_REGISTRY, laneSlot));

        // Guards against the fork block having no committed ladder, which would
        // make every swap assertion below vacuous.
        require(
            (storedLane >> 216) & 0xff > 0, "no committed lane at fork block"
        );

        vm.store(
            TEMPEST_REGISTRY,
            laneSlot,
            bytes32(
                (uint256(uint32(block.timestamp)) << 224)
                    | (storedLane & ((uint256(1) << 224) - 1))
            )
        );
    }

    /// Mirrors `Tempest.laneFor`: keccak of the ascending-sorted packed pair.
    function _lane(address tokenA, address tokenB)
        internal
        pure
        returns (bytes32)
    {
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        return keccak256(abi.encodePacked(token0, token1));
    }
}

contract TempestExecutorExposed is TempestExecutor {
    constructor(address tempest_) TempestExecutor(tempest_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract TempestExecutorTest is TestUtils, TempestFixtures {
    TempestExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), TEMPEST_FORK_BLOCK);
        executor = new TempestExecutorExposed(TEMPEST_ROUTER);
        _refreshLane(WETH_ADDR, USDC_ADDR);
        _allowTaker(address(executor));
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.tempest()), TEMPEST_ROUTER);
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(TempestExecutor__ZeroRouterAddress.selector);
        new TempestExecutorExposed(address(0));
    }

    function testDecodeParams() public view {
        (address tokenIn, address tokenOut) = executor.decodeParams(_params());

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(TempestExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(WETH_ADDR));
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
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, TEMPEST_ROUTER);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 0.1 ether;

        deal(WETH_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WETH_ADDR).approve(TEMPEST_ROUTER, amountIn);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        uint256 wethBalanceBefore =
            IERC20(WETH_ADDR).balanceOf(address(executor));

        executor.swap(amountIn, _params(), BOB);

        assertGt(IERC20(USDC_ADDR).balanceOf(BOB), usdcBalanceBefore);
        assertEq(
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(address(executor)),
            amountIn
        );
        // Tempest pulls the input straight to its vault, so the executor is
        // left holding nothing.
        assertEq(IERC20(WETH_ADDR).balanceOf(address(executor)), 0);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_tempest_weth_usdc");

        (address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function _params() internal view returns (bytes memory) {
        return abi.encodePacked(WETH_ADDR, USDC_ADDR);
    }
}

contract TempestRouterTest is TychoRouterTestSetup, TempestFixtures {
    function getForkBlock() public pure override returns (uint256) {
        return TEMPEST_FORK_BLOCK;
    }

    function testSingleSwap() public {
        uint256 amountIn = 0.1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_tempest_weth_usdc"
        );

        _refreshLane(WETH_ADDR, USDC_ADDR);
        // Executors run under delegatecall, so Tempest sees TychoRouter as the
        // taker. Flowdesk must `addTaker` the deployed router for this to work
        // against the live venue.
        _allowTaker(tychoRouterAddr);

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 wethBalanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertGt(IERC20(USDC_ADDR).balanceOf(ALICE), usdcBalanceBefore);
        assertEq(
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(ALICE), amountIn
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    /// Without the taker allowlist entry the venue rejects the fill outright.
    /// This is the failure the Tycho deployment will hit until Flowdesk calls
    /// `addTaker(tychoRouter)`.
    function testSingleSwapRevertsWhenRouterNotAllowlisted() public {
        uint256 amountIn = 0.1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_tempest_weth_usdc"
        );

        _refreshLane(WETH_ADDR, USDC_ADDR);

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        (bool success,) = tychoRouterAddr.call(callData);

        assertFalse(success, "Expected TakerNotAllowed");
    }
}
