// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "./IStructs.sol";

/// @notice Callback for executing orders through a reactor.
interface IReactorCallback {
    /// @notice Called by the reactor during the execution of an order
    /// @param resolvedOrders Has inputs and outputs
    /// @param fillData The fillData specified for an order execution
    /// @dev Must have approved each token and amount in outputs to the msg.sender
    function reactorCallback(
        ResolvedOrder[] memory resolvedOrders,
        bytes memory fillData
    ) external;
}
