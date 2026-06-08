// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {FeeRecipient} from "../lib/FeeStructs.sol";
import {IFeeCalculator} from "@interfaces/IFeeCalculator.sol";

error FeeCalculator__FeeTooHigh();
error FeeCalculator__AddressZero();

/**
 * @notice Storage-optimized struct for per-client custom fee configuration
 * @dev All fields pack into a single storage slot (6 bytes total)
 */
struct CustomFees {
    bool hasCustomFeeOnOutput; // 1 byte
    uint16 feeBpsOnOutput; // 2 bytes
    bool hasCustomFeeOnClientFee; // 1 byte
    uint16 feeBpsOnClientFee; // 2 bytes
}

/**
 * @title FeeCalculator
 * @notice Contract responsible for calculating fees on swap outputs and managing fee configuration
 * @dev This contract is called via staticCall from TychoRouter.
 *      It calculates fees and returns the values - accounting is done by the caller.
 *      It also stores all fee-related configuration.
 */
contract FeeCalculator is AccessControl, IFeeCalculator {
    uint16 private constant _MAX_FEE_BPS = 10000; // 100% max

    uint16 private _routerFeeOnOutputBps; // Router fee on output amount in basis points
    uint16 private _routerFeeOnClientFeeBps; // Router fee on client fee in basis points
    address private _routerFeeReceiver; // Address whose vault balance receives router fees

    // Per-client custom router fees (both output and client fees)
    // If set, custom values will override the default router fees for the client
    // Storage-optimized: all custom fee data for a client fits in a single slot
    mapping(address => CustomFees) private _customRouterFees;

    //keccak256("ROUTER_FEE_SETTER_ROLE")
    bytes32 public constant ROUTER_FEE_SETTER_ROLE =
        0x9939157be7760e9462f1d5a0dcad88b616ddc64138e317108b40b1cf55601348;

    event RouterFeeOnOutputUpdated(uint16 oldFeeBps, uint16 newFeeBps);
    event RouterFeeOnClientFeeUpdated(uint16 oldFeeBps, uint16 newFeeBps);
    event CustomRouterFeeOnOutputUpdated(
        address indexed client, uint16 oldFeeBps, uint16 newFeeBps
    );
    event CustomRouterFeeOnClientFeeUpdated(
        address indexed client, uint16 oldFeeBps, uint16 newFeeBps
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
     * @param amountIn The amount before fee deduction
     * @param client The client address to look up custom router fees for and to receive fees
     * @param clientFeeBps Client fee in basis points
     * @return amountOut The amount remaining after all fee deductions
     * @return feeRecipients Array of (address, feeAmount) tuples for fee distribution
     */
    function calculateFee(uint256 amountIn, address client, uint16 clientFeeBps)
        external
        view
        returns (uint256 amountOut, FeeRecipient[] memory feeRecipients)
    {
        (uint16 routerFeeOnOutputBps, uint16 routerFeeOnClientFeeBps) =
            _getFeeInfo(client);

        if (
            (clientFeeBps + routerFeeOnOutputBps > _MAX_FEE_BPS)
                || routerFeeOnClientFeeBps > _MAX_FEE_BPS
        ) {
            revert FeeCalculator__FeeTooHigh();
        }

        amountOut = amountIn;
        uint256 routerFeeOnClientFee = 0;
        uint256 clientPortion = 0;

        // Calculate client fee if > 0
        if (clientFeeBps > 0) {
            // Save numerator for later routerFeeOnClientFee calculation to avoid
            // divide-before-multiply precision loss and warning
            uint256 clientFeeNumerator = amountOut * clientFeeBps;
            uint256 totalClientFee = clientFeeNumerator / 10_000;

            // Calculate router's cut of the client fee
            if (routerFeeOnClientFeeBps > 0) {
                routerFeeOnClientFee =
                    (clientFeeNumerator * routerFeeOnClientFeeBps) / 100_000_000;
            }

            // Client gets their portion (after router's cut)
            clientPortion = totalClientFee - routerFeeOnClientFee;
        }

        uint256 totalRouterFee = routerFeeOnClientFee;

        // Calculate router fee on output amount if > 0
        if (routerFeeOnOutputBps > 0) {
            uint256 routerFeeOnOutput =
                (amountOut * routerFeeOnOutputBps) / 10000;
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
     * @return routerFeeOnOutputBps Router fee on output in basis points
     * @return routerFeeOnClientFeeBps Router fee on client fee in basis points
     */
    function _getFeeInfo(address client)
        internal
        view
        returns (uint16 routerFeeOnOutputBps, uint16 routerFeeOnClientFeeBps)
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
     * @dev Sets the router fee on output amount in basis points
     * @param feeBps The fee in basis points (e.g., 1 = 0.01%, 100 = 1%)
     */
    function setRouterFeeOnOutput(uint16 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > _MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        uint16 oldFeeBps = _routerFeeOnOutputBps;
        _routerFeeOnOutputBps = feeBps;
        emit RouterFeeOnOutputUpdated(oldFeeBps, feeBps);
    }

    /**
     * @dev Returns the current router fee on output amount in basis points
     * @return The fee in basis points
     */
    function getRouterFeeOnOutput() external view returns (uint16) {
        return _routerFeeOnOutputBps;
    }

    /**
     * @dev Sets a custom router fee on output amount for a specific client
     * @param client The client address to set the custom fee for
     * @param feeBps The fee in basis points (e.g., 1 = 0.01%, 100 = 1%)
     */
    function setCustomRouterFeeOnOutput(address client, uint16 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > _MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        CustomFees memory customFees = _customRouterFees[client];
        uint16 oldFeeBps = customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;

        customFees.feeBpsOnOutput = feeBps;
        customFees.hasCustomFeeOnOutput = true;
        _customRouterFees[client] = customFees;

        emit CustomRouterFeeOnOutputUpdated(client, oldFeeBps, feeBps);
    }

    /**
     * @dev Removes the custom router fee on output amount for a specific client, reverting to default
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

        emit CustomRouterFeeOnOutputRemoved(client);
    }

    /**
     * @dev Returns the effective router fee on output amount for a specific client
     * @param client The client address to check
     * @return The fee in basis points (custom if set, otherwise default)
     */
    function getEffectiveRouterFeeOnOutput(address client)
        external
        view
        returns (uint16)
    {
        CustomFees memory customFees = _customRouterFees[client];
        return customFees.hasCustomFeeOnOutput
            ? customFees.feeBpsOnOutput
            : _routerFeeOnOutputBps;
    }

    /**
     * @dev Sets the router platform fee on client fee in basis points
     * @param feeBps The fee in basis points (e.g., 1 = 0.01%, 100 = 1%)
     */
    function setRouterFeeOnClientFee(uint16 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > _MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        uint16 oldFeeBps = _routerFeeOnClientFeeBps;
        _routerFeeOnClientFeeBps = feeBps;
        emit RouterFeeOnClientFeeUpdated(oldFeeBps, feeBps);
    }

    /**
     * @dev Returns the current router platform fee on client fee in basis points
     * @return The fee in basis points
     */
    function getRouterFeeOnClientFee() external view returns (uint16) {
        return _routerFeeOnClientFeeBps;
    }

    /**
     * @dev Sets a custom router fee on client fee for a specific client
     * @param client The client address to set the custom fee for
     * @param feeBps The fee in basis points (e.g., 1 = 0.01%, 100 = 1%)
     */
    function setCustomRouterFeeOnClientFee(address client, uint16 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > _MAX_FEE_BPS) revert FeeCalculator__FeeTooHigh();
        CustomFees memory customFees = _customRouterFees[client];
        uint16 oldFeeBps = customFees.hasCustomFeeOnClientFee
            ? customFees.feeBpsOnClientFee
            : _routerFeeOnClientFeeBps;

        customFees.feeBpsOnClientFee = feeBps;
        customFees.hasCustomFeeOnClientFee = true;
        _customRouterFees[client] = customFees;

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

        emit CustomRouterFeeOnClientFeeRemoved(client);
    }

    /**
     * @dev Returns the effective router fee on client fee for a specific client
     * @param client The client address to check
     * @return The fee in basis points (custom if set, otherwise default)
     */
    function getEffectiveRouterFeeOnClientFee(address client)
        external
        view
        returns (uint16)
    {
        CustomFees memory customFees = _customRouterFees[client];
        return customFees.hasCustomFeeOnClientFee
            ? customFees.feeBpsOnClientFee
            : _routerFeeOnClientFeeBps;
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
