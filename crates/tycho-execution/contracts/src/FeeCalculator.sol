// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {
    EnumerableSet
} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import {IFeeCalculator, CustomFees} from "@interfaces/IFeeCalculator.sol";

error FeeCalculator__FeeTooHigh();
error FeeCalculator__AddressZero();

/**
 * @title FeeCalculator
 * @notice Contract responsible for calculating fees on swap outputs and managing fee configuration
 * @dev This contract is called via staticCall from TychoRouterV3.
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

    // Clients whose swaps keep the surplus above expectedAmountOut even
    // while positive slippage capture is enabled
    mapping(address => bool) private _positiveSlippageExempt;

    // Router fee on output in fee units for swaps whose input or output is
    // the token. Zero means no token fee.
    mapping(address => uint32) private _tokenFeeBps;

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
    event PositiveSlippageExemptionSet(address indexed client, bool exempt);
    event TokenFeeUpdated(
        address indexed token, uint32 oldFeeBps, uint32 newFeeBps
    );

    /**
     * @param routerFeeSetter Address granted ROUTER_FEE_SETTER_ROLE
     * @param routerFeeReceiver Address whose vault balance receives router fees
     * @dev The receiver is explicit rather than defaulted to `msg.sender`: when
     *      the calculator is deployed through a CREATE2 factory, the factory is
     *      the sender, and fees credited to it can never be withdrawn.
     *      Positive slippage capture starts enabled, since that is how every
     *      deployment is operated; `setPositiveSlippageEnabled` turns it off.
     */
    constructor(address routerFeeSetter, address routerFeeReceiver) {
        if (routerFeeReceiver == address(0)) {
            revert FeeCalculator__AddressZero();
        }
        _routerFeeReceiver = routerFeeReceiver;
        _positiveSlippageEnabled = true;
        // Make the role its own admin so role holders can manage their own role
        _setRoleAdmin(ROUTER_FEE_SETTER_ROLE, ROUTER_FEE_SETTER_ROLE);
        _grantRole(ROUTER_FEE_SETTER_ROLE, routerFeeSetter);
        emit RouterFeeReceiverUpdated(address(0), routerFeeReceiver);
        emit PositiveSlippageToggled(true);
    }

    /**
     * @notice Calculates all fees and slippage surplus from swap output
     * @dev Called from TychoRouterV3. Does not perform any accounting.
     *
     *      Deduction order:
     *      1. When positive slippage capture is enabled and the client is not
     *         exempt, the surplus (actualAmountOut - expectedAmountOut) is
     *         taken by the router first.
     *      2. Fees (client fee + router fees) are then calculated on
     *         the amount *after* surplus extraction (expectedAmountOut when
     *         surplus was taken, actualAmountOut otherwise).
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

        uint256 positiveSlippage = _calculatePositiveSlippage(
            feeInput.actualAmountOut, feeInput.expectedAmountOut, resolvedClient
        );

        // Fee base = actual output minus any extracted surplus.
        // When surplus is taken: feeBase = expectedAmountOut.
        // When no surplus (disabled or actual <= expected): it is zero.
        uint256 feeBase = feeInput.actualAmountOut - positiveSlippage;

        (uint256 routerFee, uint256 clientFee) = _calculateFee(
            feeBase,
            resolvedClient,
            feeInput.clientFeeBps,
            _getTokenFee(feeInput.tokenIn, feeInput.tokenOut)
        );

        feeRecipients = new FeeRecipient[](2);
        feeRecipients[0] = FeeRecipient({
            recipient: _routerFeeReceiver,
            feeAmount: routerFee + positiveSlippage
        });
        feeRecipients[1] =
            FeeRecipient({recipient: resolvedClient, feeAmount: clientFee});
    }

    /**
     * @notice Whether funds must pass through the router after the final swap instead of going directly to the receiver
     * @param clientFeeBps Client fee in basis points
     * @param client The client address to check
     * @param tokenIn The swap's input token
     * @param tokenOut The swap's output token
     * @return True if funds must pass through the router after the
     *         final swap instead of going directly to the receiver
     */
    function mustOutputThroughRouter(
        uint32 clientFeeBps,
        address client,
        address tokenIn,
        address tokenOut
    ) external view returns (bool) {
        address resolvedClient = _resolveClient(client);

        // Slippage direction is unknown before the swap, so we always
        // route funds through the router when positive slippage is enabled —
        // unless this client's surplus is never captured anyway.
        if (
            _positiveSlippageEnabled && !_positiveSlippageExempt[resolvedClient]
        ) {
            return true;
        }

        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(resolvedClient);

        if (clientFeeBps > 0) return true;
        if (routerFeeOnOutputBps > 0) return true;
        if (_getTokenFee(tokenIn, tokenOut) > 0) return true;

        return false;
    }

    /**
     * @dev Calculates fees from the fee base amount (output minus any
     *      extracted surplus).
     * @param tokenFeeBps Token fee in fee units, charged on top of the
     *        router fee on output
     * @return routerFee Total router fee (fee on output + token fee + cut of
     *         the client fee)
     * @return clientFee Client's portion of the client fee (after the router's cut)
     */
    function _calculateFee(
        uint256 feeBase,
        address client,
        uint32 clientFeeBps,
        uint32 tokenFeeBps
    ) internal view returns (uint256 routerFee, uint256 clientFee) {
        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(client);
        routerFeeOnOutputBps += tokenFeeBps;

        if (
            (clientFeeBps + routerFeeOnOutputBps > MAX_BPS)
                || routerFeeOnClientFeeBps > MAX_BPS
        ) {
            revert FeeCalculator__FeeTooHigh();
        }

        uint256 routerFeeOnClientFee = 0;

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
            clientFee = totalClientFee - routerFeeOnClientFee;
        }

        routerFee = routerFeeOnClientFee;

        // Calculate router fee on output amount if > 0
        if (routerFeeOnOutputBps > 0) {
            routerFee += (feeBase * routerFeeOnOutputBps) / MAX_BPS;
        }
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
     * @dev Calculates the positive slippage surplus, all of which goes to the router
     * @return positiveSlippage The surplus (zero if disabled, the client is
     *         exempt, or there is no surplus)
     */
    function _calculatePositiveSlippage(
        uint256 actualAmountOut,
        uint256 expectedAmountOut,
        address client
    ) internal view returns (uint256 positiveSlippage) {
        if (
            !_positiveSlippageEnabled || _positiveSlippageExempt[client]
                || actualAmountOut <= expectedAmountOut
        ) {
            return 0;
        }

        positiveSlippage = actualAmountOut - expectedAmountOut;
    }

    /**
     * @dev Returns the higher of the two tokens' fees, so a swap that touches
     *      two fee tokens (or the same token twice) pays one token fee.
     */
    function _getTokenFee(address tokenIn, address tokenOut)
        internal
        view
        returns (uint32)
    {
        uint32 feeIn = _tokenFeeBps[tokenIn];
        uint32 feeOut = _tokenFeeBps[tokenOut];
        return feeIn > feeOut ? feeIn : feeOut;
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

        if (!customFees.hasCustomFeeOnClientFee) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnOutputRemoved(client);
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

        if (!customFees.hasCustomFeeOnOutput) {
            // slither-disable-next-line unused-return
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnClientFeeRemoved(client);
    }

    /**
     * @dev Sets the router fee on output for swaps whose input or output
     *      token is `token`. Applies to every client, on top of the router
     *      fee on output. A fee of zero removes the token fee.
     * @param token The token to charge the fee on
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setTokenFee(address token, uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
        uint32 oldFeeBps = _tokenFeeBps[token];
        _tokenFeeBps[token] = feeBps;
        emit TokenFeeUpdated(token, oldFeeBps, feeBps);
    }

    /**
     * @dev Returns the token fee for `token` in fee units (zero if unset)
     */
    function getTokenFee(address token) external view returns (uint32) {
        return _tokenFeeBps[token];
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
     * @dev Exempts a client from positive slippage capture, or removes the
     *      exemption. An exempt client's swaps keep the surplus above
     *      expectedAmountOut even while capture is enabled globally.
     * @param client The client address to update
     * @param exempt True to exempt, false to capture again
     */
    function setPositiveSlippageExempt(address client, bool exempt)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        _positiveSlippageExempt[client] = exempt;
        emit PositiveSlippageExemptionSet(client, exempt);
    }

    /**
     * @dev Returns whether a client is exempt from positive slippage capture
     */
    function isPositiveSlippageExempt(address client)
        external
        view
        returns (bool)
    {
        return _positiveSlippageExempt[client];
    }
}
