pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";
import {ETH_ADDR} from "../Constants.sol";

// Etherfi
address constant EETH_ADDR = 0x35fA164735182de50811E8e2E824cFb9B6118ac2;
address constant WEETH_ADDR = 0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee;
address constant LIQUIDITY_POOL_ADDR =
    0x308861A430be4cce5502d0A12724771Fc6DaF216;
address constant REDEMPTION_MANAGER_ADDR =
    0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0;

string constant ETHERFI_PROTOCOL_SYSTEM = "etherfi";

abstract contract EtherfiDeployment is ExecutorDeployments {
    address internal immutable etherfiExecutor =
        executorAddress(ETHERFI_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_etherfiDeployment);
    }

    function _etherfiDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            ETHERFI_PROTOCOL_SYSTEM,
            "EtherfiExecutor",
            abi.encode(
                ETH_ADDR,
                EETH_ADDR,
                LIQUIDITY_POOL_ADDR,
                WEETH_ADDR,
                REDEMPTION_MANAGER_ADDR
            )
        );
    }
}
