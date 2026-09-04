pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Sky
address constant SKY_LITE_PSM =
    address(0xf6e72Db5454dd049d0788e411b06CfAF16853042);
address constant SKY_USDS_PSM_WRAPPER =
    address(0xA188EEC8F81263234dA3622A406892F3D630f98c);
address constant SKY_DAI_USDS_CONVERTER =
    address(0x3225737a9Bbb6473CB4a45b7244ACa2BeFdB276A);

string constant SKY_PROTOCOL_SYSTEM = "sky";

abstract contract SkyDeployment is ExecutorDeployments {
    address internal immutable skyExecutor =
        executorAddress(SKY_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_skyDeployment);
    }

    function _skyDeployment() private pure returns (ExecutorDeployment memory) {
        return ExecutorDeployment(
            SKY_PROTOCOL_SYSTEM,
            "SkyExecutor",
            abi.encode(
                SKY_LITE_PSM, SKY_USDS_PSM_WRAPPER, SKY_DAI_USDS_CONVERTER
            )
        );
    }
}
