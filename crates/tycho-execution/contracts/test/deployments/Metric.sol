pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant METRIC_PROTOCOL_SYSTEM = "rfq:metric";

abstract contract MetricDeployment is ExecutorDeployments {
    address internal immutable metricExecutor =
        executorAddress(METRIC_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_metricDeployment);
    }

    function _metricDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(METRIC_PROTOCOL_SYSTEM, "MetricExecutor", "");
    }
}
