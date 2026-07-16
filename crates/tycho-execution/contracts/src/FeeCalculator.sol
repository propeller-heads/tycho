// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {
    EnumerableSet
} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import {IFeeCalculator, CustomFees} from "@interfaces/IFeeCalculator.sol";

error FeeCalculator__FeeTooHigh();
error FeeCalculator__AddressZero();
error FeeCalculator__InvalidBps();

/**
 * @title FeeCalculator
 * @notice Contract responsible for calculating fees on swap outputs and managing fee configuration
 * @dev This contract is called via staticCall from TychoRouter.
 *      It calculates fees and returns the values - accounting is done by the caller.
 *      It also stores all fee-related configuration.
 *
 *      Router fees use an 8-decimal precision unit: 1 unit = 0.0001 BPS = 0.000001%.
 *      100% = 100_000_000 units. This allows sub-BPS fee rates (e.g. 1.5 BPS = 15_000 units).
 */
contract FeeCalculator is AccessControl, IFeeCalculator {
    using EnumerableSet for EnumerableSet.AddressSet;

    // 100% expressed in 8-decimal fee units (1 unit = 0.0001 BPS = 0.000001%)
    uint32 public constant MAX_BPS = 100_000_000;
    // Combined denominator when both fees use the MAX_BPS scale (MAX_BPS^2)
    uint64 public constant MAX_BPS_SQUARED = 10_000_000_000_000_000;

    uint32 private _routerFeeOnOutputBps; // Router fee on output amount in fee units
    uint32 private _routerFeeOnClientFeeBps; // Router fee on client fee in fee units
    address private _routerFeeReceiver; // Address whose vault balance receives router fees

    // Per-client custom router fees (both output and client fees)
    // If set, custom values will override the default router fees for the client
    // Storage-optimized: all custom fee data for a client fits in a single slot
    mapping(address => CustomFees) private _customRouterFees;

    // Tracks all clients that currently have at least one custom fee override
    EnumerableSet.AddressSet private _customFeeClients;

    // Positive slippage configuration
    bool private _positiveSlippageEnabled;
    uint32 private _defaultClientSlippageShareBps;

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
    event PositiveSlippageToggled(bool enabled);
    event DefaultClientSlippageShareUpdated(uint32 oldBps, uint32 newBps);
    event CustomClientSlippageShareSet(address indexed client, uint32 bps);
    event CustomClientSlippageShareRemoved(address indexed client);

    constructor(address routerFeeSetter) {
        _routerFeeReceiver = msg.sender;
        // Make the role its own admin so role holders can manage their own role
        _setRoleAdmin(ROUTER_FEE_SETTER_ROLE, ROUTER_FEE_SETTER_ROLE);
        _grantRole(ROUTER_FEE_SETTER_ROLE, routerFeeSetter);
    }

    /**
     * @notice Calculates all fees and slippage surplus from swap output
     * @dev Called from TychoRouter. Does not perform any accounting.
     *
     *      Deduction order:
     *      1. Positive slippage surplus (actualAmountOut - expectedAmountOut) is
     *         split between router and client first.
     *      2. Fees (client fee + router fees) are then calculated on
     *         expectedAmountOut, i.e. on the amount *after* surplus extraction.
     *
     *      Router fee parameters are retrieved from contract storage based on the client address.
     *      Client fee parameters are passed as function arguments.
     * @param feeInput Struct containing all fee calculation inputs
     * @return feeRecipients Array of (address, feeAmount) tuples for fee distribution
     */
    function calculateFee(FeeInput memory feeInput)
        external
        view
        returns (FeeRecipient[] memory feeRecipients)
    {
        address resolvedClient = _resolveClient(feeInput.client);

        (uint256 routerSurplus, uint256 clientSurplus) = _calculatePositiveSlippage(
            feeInput.actualAmountOut, feeInput.expectedAmountOut, resolvedClient
        );

        // Fee base = actual output minus any extracted surplus.
        // When surplus is taken: feeBase = expectedAmountOut.
        // When no surplus (disabled or actual <= expected): both cuts are zero.
        uint256 feeBase =
            feeInput.actualAmountOut - routerSurplus - clientSurplus;

        feeRecipients =
            _calculateFee(feeBase, resolvedClient, feeInput.clientFeeBps);

        // fees[0] = router, fees[1] = client (see _calculateFee).
        feeRecipients[0].feeAmount += routerSurplus;
        feeRecipients[1].feeAmount += clientSurplus;
    }

    /**
     * @notice Whether funds must pass through the router after the final swap instead of going directly to the receiver
     * @param clientFeeBps Client fee in basis points
     * @param client The client address to check
     * @return True if funds must pass through the router after the
     *         final swap instead of going directly to the receiver
     */
    function mustInterceptOutput(uint32 clientFeeBps, address client)
        external
        view
        returns (bool)
    {
        // Slippage direction is unknown before the swap, so we always
        // route funds through the router when positive slippage is enabled.
        if (_positiveSlippageEnabled) return true;

        address resolvedClient = _resolveClient(client);
        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(resolvedClient);

        if (clientFeeBps > 0) return true;
        if (routerFeeOnOutputBps > 0) return true;

        return false;
    }

    /**
     * @dev Calculates fees from the fee base amount (output minus any
     *      extracted surplus).
     * @return feeRecipients 2-element array: [0] = router, [1] = client.
     */
    function _calculateFee(uint256 feeBase, address client, uint32 clientFeeBps)
        internal
        view
        returns (FeeRecipient[] memory feeRecipients)
    {
        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(client);

        if (
            (clientFeeBps + routerFeeOnOutputBps > MAX_BPS)
                || routerFeeOnClientFeeBps > MAX_BPS
        ) {
            revert FeeCalculator__FeeTooHigh();
        }

        uint256 routerFeeOnClientFee = 0;
        uint256 clientPortion = 0;

        // Calculate client fee if > 0
        if (clientFeeBps > 0) {
            // Save numerator for later routerFeeOnClientFee calculation to avoid
            // divide-before-multiply precision loss and warning
            uint256 clientFeeNumerator = feeBase * clientFeeBps;
            uint256 totalClientFee = clientFeeNumerator / MAX_BPS;

            // Calculate router's cut of the client fee
            if (routerFeeOnClientFeeBps > 0) {
                // Both fees use the 100_000_000 scale, so denominator is 100_000_000^2
                routerFeeOnClientFee =
                    (clientFeeNumerator * routerFeeOnClientFeeBps)
                        / MAX_BPS_SQUARED;
            }

            // Client gets their portion (after router's cut)
            clientPortion = totalClientFee - routerFeeOnClientFee;
        }

        uint256 totalRouterFee = routerFeeOnClientFee;

        // Calculate router fee on output amount if > 0
        if (routerFeeOnOutputBps > 0) {
            uint256 routerFeeOnOutput =
                (feeBase * routerFeeOnOutputBps) / MAX_BPS;
            totalRouterFee += routerFeeOnOutput;
        }

        // Build fee recipients array
        feeRecipients = new FeeRecipient[](2);
        feeRecipients[0] = FeeRecipient({
            recipient: _routerFeeReceiver, feeAmount: totalRouterFee
        });
        feeRecipients[1] =
            FeeRecipient({recipient: client, feeAmount: clientPortion});
    }

    /**
     * @dev When no client signature is present (client == address(0)), fall back to tx.origin
     *      so unsigned swaps can still benefit from custom router fee rates negotiated with the
     *      client that originated the transaction.
     */
    // slither-disable-next-line tx-origin
    function _resolveClient(address client) internal view returns (address) {
        return client == address(0) ? tx.origin : client;
    }

    /**
     * @dev Calculates positive slippage surplus distribution
     * @return routerCut Router's share of the surplus (zero if disabled or no surplus)
     * @return clientCut Client's share of the surplus (zero if disabled or no surplus)
     */
    function _calculatePositiveSlippage(
        uint256 actualAmountOut,
        uint256 expectedAmountOut,
        address client
    ) internal view returns (uint256 routerCut, uint256 clientCut) {
        if (!_positiveSlippageEnabled || actualAmountOut <= expectedAmountOut) {
            return (0, 0);
        }

        uint256 surplus = actualAmountOut - expectedAmountOut;
        uint32 clientShareBps = _getClientSlippageShareBps(client);

        uint256 clientCut = (surplus * clientShareBps) / MAX_BPS;
        uint256 routerCut = surplus - clientCut;
    }

    /**
     * @dev Returns the client's slippage share: custom if set, otherwise the default
     */
    function _getClientSlippageShareBps(address client)
        internal
        view
        returns (uint32)
    {
        CustomFees memory customFees = _customRouterFees[client];
        if (customFees.hasCustomClientSlippageShare) {
            return customFees.clientSlippageShareBps;
        }
        return _defaultClientSlippageShareBps;
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
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
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
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
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

        if (
            !customFees.hasCustomFeeOnClientFee
                && !customFees.hasCustomClientSlippageShare
        ) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnOutputRemoved(client);
    }

    /**
     * @dev Returns the effective router fee on output for a specific client
     *      in fee units (100_000_000 = 100%).
     * @param client The client address to check. Pass address(0) to fall back to tx.origin.
     * @return The fee in fee units (custom if set, otherwise default)
     */
    function getEffectiveRouterFeeOnOutput(address client)
        external
        view
        returns (uint32)
    {
        CustomFees memory customFees = _customRouterFees[_resolveClient(client)];
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
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
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
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
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

        if (
            !customFees.hasCustomFeeOnOutput
                && !customFees.hasCustomClientSlippageShare
        ) {
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

    /**
     * @dev Enables or disables positive slippage capture
     * @param enabled True to enable, false to disable
     */
    function setPositiveSlippageEnabled(bool enabled)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        _positiveSlippageEnabled = enabled;
        emit PositiveSlippageToggled(enabled);
    }

    /**
     * @dev Returns whether positive slippage capture is enabled
     */
    function getPositiveSlippageEnabled() external view returns (bool) {
        return _positiveSlippageEnabled;
    }

    /**
     * @dev Sets the default client share of positive slippage
     * @param bps Share in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setDefaultClientSlippageShare(uint32 bps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (bps > MAX_BPS) {
            revert FeeCalculator__InvalidBps();
        }
        uint32 oldBps = _defaultClientSlippageShareBps;
        _defaultClientSlippageShareBps = bps;
        emit DefaultClientSlippageShareUpdated(oldBps, bps);
    }

    /**
     * @dev Returns the default client share of positive slippage in fee units
     */
    function getDefaultClientSlippageShare() external view returns (uint32) {
        return _defaultClientSlippageShareBps;
    }

    /**
     * @dev Sets a custom client share of positive slippage for a specific client
     * @param client The client address to set the custom share for
     * @param bps Share in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setCustomClientSlippageShare(address client, uint32 bps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (bps > MAX_BPS) {
            revert FeeCalculator__InvalidBps();
        }
        CustomFees memory customFees = _customRouterFees[client];
        customFees.hasCustomClientSlippageShare = true;
        customFees.clientSlippageShareBps = bps;
        _customRouterFees[client] = customFees;
        // slither-disable-next-line unused-return
        _customFeeClients.add(client);

        emit CustomClientSlippageShareSet(client, bps);
    }

    /**
     * @dev Removes the custom client slippage share for a specific client, reverting to default
     * @param client The client address to remove the custom share from
     */
    function removeCustomClientSlippageShare(address client)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        CustomFees memory customFees = _customRouterFees[client];
        customFees.hasCustomClientSlippageShare = false;
        customFees.clientSlippageShareBps = 0;
        _customRouterFees[client] = customFees;

        if (
            !customFees.hasCustomFeeOnOutput
                && !customFees.hasCustomFeeOnClientFee
        ) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomClientSlippageShareRemoved(client);
    }
}
