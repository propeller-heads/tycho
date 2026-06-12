// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {
    EnumerableSet
} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";
import {FeeRecipient} from "../lib/FeeStructs.sol";
import {IFeeCalculator, CustomFees} from "@interfaces/IFeeCalculator.sol";

error FeeCalculator__FeeTooHigh();
error FeeCalculator__AddressZero();

/**
 * @title FeeCalculator
 * @notice Contract responsible for calculating fees on swap outputs and managing fee configuration
 * @dev This contract is called via staticCall from TychoRouter.
 *      It calculates fees and returns the values - accounting is done by the caller.
 *      It also stores all fee-related configuration.
 *
 *      Router fees use an 8-decimal precision unit: 1 unit = 0.0001 BPS = 0.000001%.
 *      100% = 100_000_000 units. This allows sub-BPS fee rates (e.g. 1.5 BPS = 15_000 units).
 *
 *      The external interface (calculateFee, getEffectiveRouterFeeOnOutput) preserves legacy
 *      BPS semantics (10_000 = 100%) for compatibility with TychoRouter and Dispatcher.
 */
contract FeeCalculator is AccessControl, IFeeCalculator {
    using EnumerableSet for EnumerableSet.AddressSet;

    // 100% expressed in 8-decimal fee units (1 unit = 0.0001 BPS = 0.000001%)
    uint32 public constant MAX_FEE_BPS = 100_000_000;
    // Combined denominator when both fees use the MAX_FEE_BPS scale (MAX_FEE_BPS^2)
    uint64 public constant MAX_FEE_BPS_SQUARED = 10_000_000_000_000_000;

    uint32 private _routerFeeOnOutputBps; // Router fee on output amount in fee units
    uint32 private _routerFeeOnClientFeeBps; // Router fee on client fee in fee units
    address private _routerFeeReceiver; // Address whose vault balance receives router fees

    // Per-client custom router fees (both output and client fees)
    // If set, custom values will override the default router fees for the client
    // Storage-optimized: all custom fee data for a client fits in a single slot
    mapping(address => CustomFees) private _customRouterFees;

    // Tracks all clients that currently have at least one custom fee override
    EnumerableSet.AddressSet private _customFeeClients;

    //keccak256("ROUTER_FEE_SETTER_ROLE")
    bytes32 public constant ROUTER_FEE_SETTER_ROLE =
        0x9939157be7760e9462f1d5a0dcad88b616ddc64138e317108b40b1cf55601348;

    event RouterFeeOnOutputUpdated(uint32 oldFeeBps, uint32 newFeeBps);
    event RouterFeeOnClientFeeUpdated(uint32 oldFeeBps, uint32 newFeeBps);
    event CustomRouterFeeOnOutputUpdated(
        address indexed client, uint32 oldFeeBps, uint32 newFeeBps
    );
    event CustomRouterFeeOnClientFeeUpdated(
        address indexed client, uint32 oldFeeBps, uint32 newFeeBps
    );
    event CustomRouterFeeOnOutputRemoved(address indexed client);
    event CustomRouterFeeOnClientFeeRemoved(address indexed client);
    event RouterFeeReceiverUpdated(
        address indexed oldReceiver, address indexed newReceiver
    );

    constructor(address routerFeeSetter) {
        _routerFeeReceiver = msg.sender;

        // Make the role its own admin so role holders can manage their own role
        _setRoleAdmin(ROUTER_FEE_SETTER_ROLE, ROUTER_FEE_SETTER_ROLE);
        _grantRole(ROUTER_FEE_SETTER_ROLE, routerFeeSetter);
    }

    /**
     * @notice Calculates fees from the swap output amount
     * @dev Called from TychoRouter. Does not perform any accounting.
     *      Router fee parameters are retrieved from contract storage based on the client address.
     *      Client fee parameters are passed as function arguments.
     *      clientFeeBps uses the legacy BPS scale (10000 = 100%). Internally it is scaled to the
     *      same 8-decimal unit system used for router fees (100_000_000 = 100%).
     * @param amountIn The amount before fee deduction
     * @param client The client address to look up custom router fees for and to receive fees
     * @param clientFeeBps Client fee in basis points (10000 = 100%)
     * @return amountOut The amount remaining after all fee deductions
     * @return feeRecipients Array of (address, feeAmount) tuples for fee distribution
     */
    function calculateFee(uint256 amountIn, address client, uint16 clientFeeBps)
        external
        view
        returns (uint256 amountOut, FeeRecipient[] memory feeRecipients)
    {
        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(client);

        // Scale clientFeeBps from legacy scale (10_000 = 100%) to internal scale
        // (100_000_000 = 100%) so both fee types can be compared and combined.
        uint32 scaledClientFeeBps = uint32(clientFeeBps) * 10_000;

        if (
            (scaledClientFeeBps + routerFeeOnOutputBps > MAX_FEE_BPS)
                || routerFeeOnClientFeeBps > MAX_FEE_BPS
        ) {
            revert FeeCalculator__FeeTooHigh();
        }

        amountOut = amountIn;
        uint256 routerFeeOnClientFee = 0;
        uint256 clientPortion = 0;

        // Calculate client fee if > 0
        if (scaledClientFeeBps > 0) {
            // Save numerator for later routerFeeOnClientFee calculation to avoid
            // divide-before-multiply precision loss and warning
            uint256 clientFeeNumerator = amountOut * scaledClientFeeBps;
            uint256 totalClientFee = clientFeeNumerator / MAX_FEE_BPS;

            // Calculate router's cut of the client fee
            if (routerFeeOnClientFeeBps > 0) {
                // Both fees use the 100_000_000 scale, so denominator is 100_000_000^2
                routerFeeOnClientFee =
                    (clientFeeNumerator * routerFeeOnClientFeeBps)
                        / MAX_FEE_BPS_SQUARED;
            }

            // Client gets their portion (after router's cut)
            clientPortion = totalClientFee - routerFeeOnClientFee;
        }

        uint256 totalRouterFee = routerFeeOnClientFee;

        // Calculate router fee on output amount if > 0
        if (routerFeeOnOutputBps > 0) {
            uint256 routerFeeOnOutput =
                (amountOut * routerFeeOnOutputBps) / MAX_FEE_BPS;
            totalRouterFee += routerFeeOnOutput;
        }

        // Update amountOut considering both fees
        amountOut -= (clientPortion + totalRouterFee);

        // Build fee recipients array
        feeRecipients = new FeeRecipient[](2);
        feeRecipients[0] = FeeRecipient({
            recipient: _routerFeeReceiver, feeAmount: totalRouterFee
        });
        feeRecipients[1] =
            FeeRecipient({recipient: client, feeAmount: clientPortion});

        return (amountOut, feeRecipients);
    }

    /**
     * @notice Gets fee information for a specific client
     * @dev Returns custom fees if set for the client, otherwise returns default fees
     * @param client The client address to check
     * @return routerFeeOnOutputBps Router fee on output in fee units
     * @return routerFeeOnClientFeeBps Router fee on client fee in fee units
     */
    function _getFeeInfo(address client)
        internal
        view
        returns (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps)
    {
        CustomFees memory customFees = _customRouterFees[client];

        routerFeeOnOutputBps = customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;

        routerFeeOnClientFeeBps = customFees.hasCustomFeeOnClientFee
            ? customFees.feeBpsOnClientFee
            : _routerFeeOnClientFeeBps;
    }

    /**
     * @dev Sets the router fee on output amount in fee units
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setRouterFeeOnOutput(uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        uint32 oldFeeBps = _routerFeeOnOutputBps;
        _routerFeeOnOutputBps = feeBps;
        emit RouterFeeOnOutputUpdated(oldFeeBps, feeBps);
    }

    /**
     * @dev Returns the current router fee on output amount in fee units
     */
    function getRouterFeeOnOutput() external view returns (uint32) {
        return _routerFeeOnOutputBps;
    }

    /**
     * @dev Sets a custom router fee on output amount for a specific client
     * @param client The client address to set the custom fee for
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setCustomRouterFeeOnOutput(address client, uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        CustomFees memory customFees = _customRouterFees[client];
        uint32 oldFeeBps = customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;

        customFees.feeBpsOnOutput = feeBps;
        customFees.hasCustomFeeOnOutput = true;
        _customRouterFees[client] = customFees;
        // slither-disable-next-line unused-return
        _customFeeClients.add(client);

        emit CustomRouterFeeOnOutputUpdated(client, oldFeeBps, feeBps);
    }

    /**
     * @dev Removes the custom router fee on output amount for a specific client, reverting to
     *      default
     * @param client The client address to remove the custom fee from
     */
    function removeCustomRouterFeeOnOutput(address client)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        CustomFees memory customFees = _customRouterFees[client];
        customFees.hasCustomFeeOnOutput = false;
        customFees.feeBpsOnOutput = 0;
        _customRouterFees[client] = customFees;

        if (!customFees.hasCustomFeeOnClientFee) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnOutputRemoved(client);
    }

    /**
     * @dev Returns the effective router fee on output for a specific client in legacy BPS scale
     *      (10_000 = 100%), for interface compatibility with TychoRouter and Dispatcher.
     * @dev For full-precision value use getEffectiveRouterFeeOnOutputScaled.
     * @param client The client address to check
     * @return Zero if no fee is set; otherwise the fee in legacy BPS (rounded down, minimum 1).
     */
    function getEffectiveRouterFeeOnOutput(address client)
        external
        view
        returns (uint16)
    {
        CustomFees memory customFees = _customRouterFees[client];
        uint32 fee = customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;
        if (fee == 0) return 0;
        // Convert from internal scale (100_000_000 = 100%) to legacy BPS scale (10_000 = 100%).
        // Return at least 1 so callers can detect that a fee is active.
        uint32 legacyBps = fee / 10_000;
        return uint16(legacyBps > 0 ? legacyBps : 1);
    }

    /**
     * @dev Returns the effective router fee on output for a specific client in fee units
     *      (100_000_000 = 100%).
     * @param client The client address to check
     * @return The fee in fee units (custom if set, otherwise default)
     */
    function getEffectiveRouterFeeOnOutputScaled(address client)
        external
        view
        returns (uint32)
    {
        CustomFees memory customFees = _customRouterFees[client];
        return customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;
    }

    /**
     * @dev Sets the router platform fee on client fee in fee units
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setRouterFeeOnClientFee(uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        uint32 oldFeeBps = _routerFeeOnClientFeeBps;
        _routerFeeOnClientFeeBps = feeBps;
        emit RouterFeeOnClientFeeUpdated(oldFeeBps, feeBps);
    }

    /**
     * @dev Returns the current router platform fee on client fee in fee units
     */
    function getRouterFeeOnClientFee() external view returns (uint32) {
        return _routerFeeOnClientFeeBps;
    }

    /**
     * @dev Sets a custom router fee on client fee for a specific client
     * @param client The client address to set the custom fee for
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setCustomRouterFeeOnClientFee(address client, uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        CustomFees memory customFees = _customRouterFees[client];
        uint32 oldFeeBps = customFees.hasCustomFeeOnClientFee
            ? customFees.feeBpsOnClientFee
            : _routerFeeOnClientFeeBps;

        customFees.feeBpsOnClientFee = feeBps;
        customFees.hasCustomFeeOnClientFee = true;
        _customRouterFees[client] = customFees;
        // slither-disable-next-line unused-return
        _customFeeClients.add(client);

        emit CustomRouterFeeOnClientFeeUpdated(client, oldFeeBps, feeBps);
    }

    /**
     * @dev Removes the custom router fee on client fee for a specific client, reverting to default
     * @param client The client address to remove the custom fee from
     */
    function removeCustomRouterFeeOnClientFee(address client)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        CustomFees memory customFees = _customRouterFees[client];
        customFees.hasCustomFeeOnClientFee = false;
        customFees.feeBpsOnClientFee = 0;
        _customRouterFees[client] = customFees;

        if (!customFees.hasCustomFeeOnOutput) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnClientFeeRemoved(client);
    }

    /**
     * @dev Returns the effective router fee on client fee for a specific client in fee units
     * @param client The client address to check
     * @return The fee in fee units (custom if set, otherwise default)
     */
    function getEffectiveRouterFeeOnClientFee(address client)
        external
        view
        returns (uint32)
    {
        CustomFees memory customFees = _customRouterFees[client];
        return customFees.hasCustomFeeOnClientFee
            ? customFees.feeBpsOnClientFee
            : _routerFeeOnClientFeeBps;
    }

    /**
     * @notice Returns a page of clients with custom fee overrides and their current settings
     * @param start Index to start reading from (0-indexed)
     * @param count Maximum number of entries to return
     * @return clients Addresses of clients with at least one custom fee
     * @return fees Custom fee configuration for each client (parallel array)
     */
    function getAllClientFees(uint256 start, uint256 count)
        external
        view
        returns (address[] memory clients, CustomFees[] memory fees)
    {
        uint256 total = _customFeeClients.length();
        if (start >= total) return (new address[](0), new CustomFees[](0));
        uint256 remaining = total - start;
        uint256 size = count < remaining ? count : remaining;
        clients = new address[](size);
        fees = new CustomFees[](size);
        for (uint256 i = 0; i < size; i++) {
            address client = _customFeeClients.at(start + i);
            clients[i] = client;
            fees[i] = _customRouterFees[client];
        }
    }

    /**
     * @dev Sets the address that receives router fees
     * @param routerFeeReceiver The address to receive router fees
     */
    function setRouterFeeReceiver(address routerFeeReceiver)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (routerFeeReceiver == address(0)) {
            revert FeeCalculator__AddressZero();
        }
        address oldReceiver = _routerFeeReceiver;
        _routerFeeReceiver = routerFeeReceiver;
        emit RouterFeeReceiverUpdated(oldReceiver, routerFeeReceiver);
    }

    /**
     * @dev Returns the current router fee receiver address
     */
    function getRouterFeeReceiver() external view returns (address) {
        return _routerFeeReceiver;
    }
}
