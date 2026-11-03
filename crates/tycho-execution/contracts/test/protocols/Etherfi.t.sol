pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "../TestUtils.sol";
import "@src/executors/EtherfiExecutor.sol";
import {Constants} from "../Constants.sol";
import {Vm} from "forge-std/Vm.sol";

// Match the `ethereum-etherfi` snapshot block so indexing and execution
// tests use the same contract implementations and state.
uint256 constant ETHERFI_FORK_BLOCK = 25940000;

// `LiquidityPool` slot 207: `totalValueOutOfLp` in the low 128 bits, `totalValueInLp` in the
// high 128 bits.
uint256 constant LIQUIDITY_POOL_VALUE_SLOT = 207;

interface IEtherfiRedemptionManagerView {
    function tokenToRedemptionInfo(address token)
        external
        view
        returns (
            uint64 capacity,
            uint64 remaining,
            uint64 lastRefill,
            uint64 refillRate,
            uint16 exitFeeSplitToTreasuryInBps,
            uint16 exitFeeInBps,
            uint16 lowWatermarkInBpsOfTvl
        );

    function totalRedeemableAmount(address token)
        external
        view
        returns (uint256);
}

/// Raises the pool's liquid ether until `amount` is redeemable, and reverts if it is not.
///
/// The fork state has insufficient liquidity above the redemption floor.
/// Sets `totalValueInLp` and the pool's ETH balance to cover `amount` while
/// preserving `totalValueOutOfLp`, then checks the redemption manager's limit.
function openEtherfiRedemptions(
    Vm vm,
    address liquidityPool,
    address redemptionManager,
    address ethSentinel,
    uint256 amount
) {
    bytes32 slot = bytes32(LIQUIDITY_POOL_VALUE_SLOT);
    uint256 totalValueOutOfLp =
        uint256(vm.load(liquidityPool, slot)) & type(uint128).max;
    (,,,,,, uint16 lowWatermarkInBpsOfTvl) = IEtherfiRedemptionManagerView(
            redemptionManager
        ).tokenToRedemptionInfo(ethSentinel);

    // Solve `inLp - bps * (inLp + outOfLp) / 10000 = amount` for `inLp`, rounded up.
    uint256 bps = uint256(lowWatermarkInBpsOfTvl);
    uint256 totalValueInLp =
        (amount * 10000 + bps * totalValueOutOfLp) / (10000 - bps) + 1;
    require(
        totalValueInLp <= type(uint128).max, "totalValueInLp overflows uint128"
    );

    vm.store(
        liquidityPool,
        slot,
        bytes32((totalValueInLp << 128) | totalValueOutOfLp)
    );
    // `_checkTotalValueInLp` requires the pool to hold the ether it accounts for.
    vm.deal(liquidityPool, totalValueInLp);

    require(
        IEtherfiRedemptionManagerView(redemptionManager)
            .totalRedeemableAmount(ethSentinel) >= amount,
        "redemptions still closed"
    );
}

contract EtherfiExecutorExposed is EtherfiExecutor {
    constructor(
        address _ethAddress,
        address _eethAddress,
        address _liquidityPoolAddress,
        address _weethAddress,
        address _redemptionManagerAddress
    )
        EtherfiExecutor(
            _ethAddress,
            _eethAddress,
            _liquidityPoolAddress,
            _weethAddress,
            _redemptionManagerAddress
        )
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (EtherfiDirection direction)
    {
        return _decodeData(data);
    }

    receive() external payable {}
}

contract EtherfiExecutorTest is Constants, TestUtils {
    EtherfiExecutorExposed etherfiExposed;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), ETHERFI_FORK_BLOCK);
        etherfiExposed = new EtherfiExecutorExposed(
            ETH_ADDR,
            EETH_ADDR,
            LIQUIDITY_POOL_ADDR,
            WEETH_ADDR,
            REDEMPTION_MANAGER_ADDR
        );
    }

    function _mintEethToExecutor(uint256 amountIn)
        internal
        returns (uint256 minted)
    {
        bytes memory protocolData = abi.encodePacked(EtherfiDirection.EthToEeth);

        vm.deal(address(this), amountIn);
        uint256 balBefore = IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));
        etherfiExposed.swap{value: amountIn}(
            amountIn, protocolData, address(etherfiExposed)
        );
        uint256 balAfter = IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));
        minted = balAfter - balBefore;
    }

    function testConstructorNotAContract() public {
        vm.expectRevert(EtherfiExecutor__NotAContract.selector);
        new EtherfiExecutorExposed(
            ETH_ADDR,
            address(0x1),
            LIQUIDITY_POOL_ADDR,
            WEETH_ADDR,
            REDEMPTION_MANAGER_ADDR
        );
    }

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(EtherfiDirection.EethToWeeth);

        EtherfiDirection direction = etherfiExposed.decodeParams(params);

        assertEq(uint8(direction), uint8(EtherfiDirection.EethToWeeth));
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(EtherfiDirection.EethToWeeth, true);

        vm.expectRevert(EtherfiExecutor__InvalidDataLength.selector);
        etherfiExposed.decodeParams(invalidParams);
    }

    function testSwapEthToEeth() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData = abi.encodePacked(EtherfiDirection.EthToEeth);

        vm.deal(address(this), amountIn);
        uint256 balanceBefore =
            IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));

        etherfiExposed.swap{value: amountIn}(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));

        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapEethToWeeth() public {
        uint256 minted = _mintEethToExecutor(1 ether);
        bytes memory protocolData =
            abi.encodePacked(EtherfiDirection.EethToWeeth);

        // Approval normally handled by Dispatcher._approveIfNeeded in router flow
        vm.prank(address(etherfiExposed));
        IERC20(EETH_ADDR).approve(WEETH_ADDR, type(uint256).max);

        uint256 balanceBefore =
            IERC20(WEETH_ADDR).balanceOf(address(etherfiExposed));
        etherfiExposed.swap(minted, protocolData, BOB);
        uint256 balanceAfter =
            IERC20(WEETH_ADDR).balanceOf(address(etherfiExposed));

        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapWeethToEeth() public {
        uint256 minted = _mintEethToExecutor(1 ether);
        bytes memory wrapData = abi.encodePacked(EtherfiDirection.EethToWeeth);

        // Approval normally handled by Dispatcher._approveIfNeeded in router flow
        vm.prank(address(etherfiExposed));
        IERC20(EETH_ADDR).approve(WEETH_ADDR, type(uint256).max);

        uint256 weethBefore =
            IERC20(WEETH_ADDR).balanceOf(address(etherfiExposed));
        etherfiExposed.swap(minted, wrapData, address(etherfiExposed));
        uint256 weethAmount =
            IERC20(WEETH_ADDR).balanceOf(address(etherfiExposed)) - weethBefore;

        bytes memory unwrapData = abi.encodePacked(EtherfiDirection.WeethToEeth);

        uint256 balanceBefore =
            IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));
        etherfiExposed.swap(weethAmount, unwrapData, BOB);
        uint256 balanceAfter =
            IERC20(EETH_ADDR).balanceOf(address(etherfiExposed));

        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapEethToEth() public {
        openEtherfiRedemptions(
            vm, LIQUIDITY_POOL_ADDR, REDEMPTION_MANAGER_ADDR, ETH_ADDR, 10 ether
        );
        uint256 minted = _mintEethToExecutor(1 ether);
        bytes memory protocolData = abi.encodePacked(EtherfiDirection.EethToEth);

        vm.prank(address(etherfiExposed));
        IERC20(EETH_ADDR).approve(REDEMPTION_MANAGER_ADDR, type(uint256).max);

        uint256 ethBefore = address(etherfiExposed).balance;
        etherfiExposed.swap(minted, protocolData, address(etherfiExposed));
        uint256 ethAfter = address(etherfiExposed).balance;

        assertGt(ethAfter, ethBefore, "ETH should be received");
        // eETH is share-based: up to 1 wei dust may remain after redemption
        // due to share rounding. The Dispatcher tracks output via balance-diff
        // so this dust does not affect swap accounting.
        assertLe(
            IERC20(EETH_ADDR).balanceOf(address(etherfiExposed)),
            1,
            "at most 1 wei eETH dust may remain"
        );
    }
}

contract TychoRouterForEtherfiTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return ETHERFI_FORK_BLOCK;
    }

    function testSingleEtherfiUnwrapIntegration() public {
        // weeth -> (unwrap) -> eeth -> (RedemptionManager) -> eth
        openEtherfiRedemptions(
            vm, LIQUIDITY_POOL_ADDR, REDEMPTION_MANAGER_ADDR, ETH_ADDR, 10 ether
        );
        deal(WEETH_ADDR, BOB, 1 ether);
        uint256 balanceBefore = BOB.balance;

        vm.startPrank(BOB);
        IERC20(WEETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_etherfi_unwrap_weeth"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = BOB.balance;

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WEETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }

    function testSingleEtherfiWrapIntegration() public {
        // eth -> (deposit) -> eeth -> (wrap) -> weeth
        IERC20 weeth = IERC20(WEETH_ADDR);
        deal(BOB, 1 ether);
        uint256 balanceBefore = weeth.balanceOf(BOB);

        vm.startPrank(BOB);

        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_etherfi_wrap_eeth"
        );
        (bool success,) = tychoRouterAddr.call{value: 1 ether}(callData);

        uint256 balanceAfter = weeth.balanceOf(BOB);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WEETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }
}
