pragma solidity ^0.8.26;

import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {PropAMMFallbackRouter} from "@src/fallback/PropAMMFallbackRouter.sol";
import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";
import {FLUIDV1_LIQUIDITY} from "./FluidV1.sol";
import {UNISWAP_V3_STATIC_QUOTER} from "./UniswapV3.sol";
import {POOL_MANAGER} from "./UniswapV4.sol";

string constant PROP_AMM_FALLBACK_PROTOCOL_SYSTEM = "fallback";

abstract contract PropAMMFallbackDeployment is ExecutorDeployments {
    address internal immutable propAMMFallbackExecutor =
        executorAddress(PROP_AMM_FALLBACK_PROTOCOL_SYSTEM);
    PropAMMFallbackRouter internal propAMMFallbackRouter;

    constructor() {
        deploymentHooks.push(_propAMMFallbackDeployment);
    }

    function _propAMMFallbackDeployment()
        private
        returns (ExecutorDeployment memory)
    {
        propAMMFallbackRouter = new PropAMMFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
        return ExecutorDeployment(
            PROP_AMM_FALLBACK_PROTOCOL_SYSTEM,
            "PropAMMFallbackExecutor",
            abi.encode(address(propAMMFallbackRouter))
        );
    }
}
