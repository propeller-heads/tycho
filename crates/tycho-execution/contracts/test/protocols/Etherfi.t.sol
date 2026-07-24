pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "../TestUtils.sol";
import "@src/executors/EtherfiExecutor.sol";
import {Constants} from "../Constants.sol";

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
        uint256 forkBlock = 24332199;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
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
        return 24332199;
    }

    function testSingleEtherfiUnwrapIntegration() public {
        // weeth -> (unwrap) -> eeth -> (RedemptionManager) -> eth
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
