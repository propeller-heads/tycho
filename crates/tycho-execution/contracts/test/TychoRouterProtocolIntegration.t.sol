pragma solidity ^0.8.26;

import "./TychoRouterTestSetup.sol";
import "./protocols/UniswapV4Utils.sol";
import "@src/executors/BebopExecutor.sol";
import {ExecutorDeployment} from "./ExecutorDeployment.sol";
import {ALICE, DAI_ADDR, PERMIT2_ADDRESS} from "./Constants.sol";
import {BalancerV2Deployment} from "./deployments/BalancerV2.sol";
import {CurveDeployment} from "./deployments/Curve.sol";
import {EkuboDeployment} from "./deployments/Ekubo.sol";
import {UniswapV2Deployment} from "./deployments/UniswapV2.sol";
import {UniswapV4Deployment} from "./deployments/UniswapV4.sol";

contract TychoRouterTestProtocolIntegration is
    TychoRouterTestSetup,
    BalancerV2Deployment,
    CurveDeployment,
    EkuboDeployment,
    UniswapV2Deployment,
    UniswapV4Deployment
{
    function testMultiProtocolIntegration() public {
        // Test created with calldata from our router encoder.
        //
        //  DAI ─(USV2)─> WETH ─(bal)─> WBTC ─(curve)─> USDT ─(ekubo)─> ETH ─(USV4)─> USDC

        deal(DAI_ADDR, ALICE, 1500 ether);
        uint256 balanceBefore = address(ALICE).balance;

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData = loadCallDataFromFile("test_multi_protocol");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = address(ALICE).balance;

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 732214216964381330);
    }
}
