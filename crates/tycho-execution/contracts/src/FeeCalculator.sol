// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {
    EnumerableSet
} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import {IFeeCalculator, CustomFees} from "@interfaces/IFeeCalculator.sol";

error FeeCalculator__FeeTooHigh();
error FeeCalculator__ShareTooHigh();
error FeeCalculator__RouterPositiveSlippageBpsTooHigh();
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

    struct CustomRouterPositiveSlippageBps {
        bool isSet;
        uint32 slippageBps;
    }

    struct TokenPairFee {
        bool isSet;
        uint32 feeBps;
    }

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

    // Part of the surplus the router captures, in fee units (MAX_BPS = all)
    uint32 private _routerPositiveSlippageBps;

    // Per-client router positive slippage bps. A set value replaces the global router positive slippage bps
    // for the client, so it can be zero.
    mapping(address => CustomRouterPositiveSlippageBps) private
        _customRouterPositiveSlippageBps;

    // Each client's share of the captured surplus in fee units. The router
    // keeps the rest, so the default of zero leaves the full surplus to it.
    mapping(address => uint32) private _clientPositiveSlippageShareBps;

    // Router fee on output for swaps between two tokens, in both
    // directions. Keyed by the lower token address, then the higher one.
    // A set pair fee replaces the default router fee on output, so it can
    // be zero.
    mapping(address => mapping(address => TokenPairFee)) private _tokenPairFees;
    uint256 private _tokenPairFeeCount;

    bytes32 public constant ROUTER_FEE_SETTER_ROLE =
        keccak256("ROUTER_FEE_SETTER_ROLE");

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
    event RouterPositiveSlippageBpsUpdated(
        uint32 oldSlippageBps, uint32 newSlippageBps
    );
    event CustomRouterPositiveSlippageBpsUpdated(
        address indexed client, uint32 oldSlippageBps, uint32 newSlippageBps
    );
    event CustomRouterPositiveSlippageBpsRemoved(address indexed client);
    event ClientPositiveSlippageShareUpdated(
        address indexed client, uint32 oldShareBps, uint32 newShareBps
    );
    event TokenPairFeeUpdated(
        address indexed token0, address indexed token1, uint32 newFeeBps
    );
    event TokenPairFeeRemoved(address indexed token0, address indexed token1);

    /**
     * @param routerFeeSetter Address granted ROUTER_FEE_SETTER_ROLE
     * @param routerFeeReceiver Address whose vault balance receives router fees
     * @dev The receiver is explicit rather than defaulted to `msg.sender`: when
     *      the calculator is deployed through a CREATE2 factory, the factory is
     *      the sender, and fees credited to it can never be withdrawn.
     *      The router captures 100% of the surplus to start, since that is
     *      how every deployment is operated.
     */
    constructor(address routerFeeSetter, address routerFeeReceiver) {
        if (routerFeeReceiver == address(0)) {
            revert FeeCalculator__AddressZero();
        }
        _routerFeeReceiver = routerFeeReceiver;
        _routerPositiveSlippageBps = MAX_BPS;
        // Make the role its own admin so role holders can manage their own role
        _setRoleAdmin(ROUTER_FEE_SETTER_ROLE, ROUTER_FEE_SETTER_ROLE);
        _grantRole(ROUTER_FEE_SETTER_ROLE, routerFeeSetter);
        emit RouterFeeReceiverUpdated(address(0), routerFeeReceiver);
        emit RouterPositiveSlippageBpsUpdated(0, MAX_BPS);
    }

    /**
     * @notice Calculates all fees and slippage surplus from swap output
     * @dev Called from TychoRouterV3. Does not perform any accounting.
     *
     *      Deduction order:
     *      1. The router captures part of the surplus
     *         (actualAmountOut - expectedAmountOut) first: the client's
     *         custom router positive slippage bps if set, otherwise the global one. The client gets
     *         its share of the captured amount and the router keeps the
     *         rest. The uncaptured surplus stays in the swap output.
     *      2. Fees (client fee + router fees) are then calculated on
     *         the amount *after* surplus extraction.
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
        uint256 clientPositiveSlippage = positiveSlippage == 0
            ? 0
            : positiveSlippage * _clientPositiveSlippageShareBps[resolvedClient]
                / MAX_BPS;

        // Fee base = actual output minus the captured surplus.
        uint256 feeBase = feeInput.actualAmountOut - positiveSlippage;

        (uint256 routerFee, uint256 clientFee) = _calculateFee(
            feeBase,
            resolvedClient,
            feeInput.clientFeeBps,
            feeInput.tokenIn,
            feeInput.tokenOut
        );

        feeRecipients = new FeeRecipient[](2);
        feeRecipients[0] = FeeRecipient({
            recipient: _routerFeeReceiver,
            feeAmount: routerFee + positiveSlippage - clientPositiveSlippage
        });
        feeRecipients[1] = FeeRecipient({
            recipient: resolvedClient,
            feeAmount: clientFee + clientPositiveSlippage
        });
    }

    /**
     * @notice Whether funds must pass through the router after the final swap instead of going directly to the receiver
     * @param clientFeeBps Client fee in basis points
     * @param client The client address to check
     * @return True if funds must pass through the router after the
     *         final swap instead of going directly to the receiver
     */
    function mustOutputThroughRouter(uint32 clientFeeBps, address client)
        external
        view
        returns (bool)
    {
        address resolvedClient = _resolveClient(client);

        // Slippage direction is unknown before the swap, so we always
        // route funds through the router when the client's surplus can be
        // captured.
        if (_getRouterPositiveSlippageBps(resolvedClient) > 0) return true;

        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(resolvedClient);

        if (clientFeeBps > 0) return true;
        if (routerFeeOnOutputBps > 0) return true;
        // The swap tokens are not known here, so any pair fee might apply.
        if (_tokenPairFeeCount > 0) return true;

        return false;
    }

    /**
     * @dev Calculates fees from the fee base amount (output minus any
     *      extracted surplus).
     * @return routerFee Total router fee (fee on output + cut of the client
     *         fee)
     * @return clientFee Client's portion of the client fee (after the router's cut)
     */
    function _calculateFee(
        uint256 feeBase,
        address client,
        uint32 clientFeeBps,
        address tokenIn,
        address tokenOut
    ) internal view returns (uint256 routerFee, uint256 clientFee) {
        (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps) =
            _getFeeInfo(client);
        routerFeeOnOutputBps =
            _applyTokenPairFee(routerFeeOnOutputBps, client, tokenIn, tokenOut);

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
    function _resolveClient(address client) internal view returns (address) {
        return client == address(0) ? tx.origin : client;
    }

    /**
     * @dev Calculates the captured part of the positive slippage surplus
     * @return positiveSlippage The captured part of the surplus (zero if
     *         the client's router positive slippage bps is zero or there is no surplus)
     */
    function _calculatePositiveSlippage(
        uint256 actualAmountOut,
        uint256 expectedAmountOut,
        address client
    ) internal view returns (uint256 positiveSlippage) {
        if (actualAmountOut <= expectedAmountOut) {
            return 0;
        }

        positiveSlippage = (actualAmountOut - expectedAmountOut)
            * _getRouterPositiveSlippageBps(client) / MAX_BPS;
    }

    /**
     * @dev Returns the client's custom router positive slippage bps if set, otherwise the
     *      global router positive slippage bps
     */
    function _getRouterPositiveSlippageBps(address client)
        internal
        view
        returns (uint32)
    {
        CustomRouterPositiveSlippageBps memory customSlippage =
            _customRouterPositiveSlippageBps[client];
        return customSlippage.isSet
            ? customSlippage.slippageBps
            : _routerPositiveSlippageBps;
    }

    function _getTokenPairFee(address tokenA, address tokenB)
        internal
        view
        returns (TokenPairFee memory)
    {
        (address token0, address token1) = _sortTokens(tokenA, tokenB);
        return _tokenPairFees[token0][token1];
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
     * @dev Applies the pair fee to the client's router fee on output. A set
     *      pair fee replaces the default fee. For a client with a custom fee
     *      on output, the lower of the custom fee and the pair fee applies.
     */
    function _applyTokenPairFee(
        uint32 routerFeeOnOutputBps,
        address client,
        address tokenIn,
        address tokenOut
    ) internal view returns (uint32) {
        TokenPairFee memory pairFee = _getTokenPairFee(tokenIn, tokenOut);
        if (!pairFee.isSet) return routerFeeOnOutputBps;
        if (!_customRouterFees[client].hasCustomFeeOnOutput) {
            return pairFee.feeBps;
        }
        return pairFee.feeBps < routerFeeOnOutputBps
            ? pairFee.feeBps
            : routerFeeOnOutputBps;
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
        // slither-disable-next-line unused-return add returns false for a client already in the set, which is fine here.
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
            // slither-disable-next-line unused-return The client is in the set while it has a custom fee, so remove cannot fail.
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
        // slither-disable-next-line unused-return add returns false for a client already in the set, which is fine here.
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
            // slither-disable-next-line unused-return The client is in the set while it has a custom fee, so remove cannot fail.
            _customFeeClients.remove(client);
        }

        emit CustomRouterFeeOnClientFeeRemoved(client);
    }

    /**
     * @dev Sets the router fee on output for swaps between `tokenA` and
     *      `tokenB`, in both directions. It replaces the default router fee
     *      on output. For a client with a custom fee on output, the lower of
     *      the two applies. A fee of zero makes the pair free of the router
     *      fee on output.
     * @param feeBps Fee in fee units (1 unit = 0.0001 BPS; 100_000_000 = 100%)
     */
    function setTokenPairFee(address tokenA, address tokenB, uint32 feeBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (feeBps > MAX_BPS) revert FeeCalculator__FeeTooHigh();
        (address token0, address token1) = _sortTokens(tokenA, tokenB);
        if (!_tokenPairFees[token0][token1].isSet) ++_tokenPairFeeCount;
        _tokenPairFees[token0][token1] =
            TokenPairFee({isSet: true, feeBps: feeBps});
        emit TokenPairFeeUpdated(token0, token1, feeBps);
    }

    /**
     * @dev Removes the pair fee, so swaps between `tokenA` and `tokenB` pay
     *      the default or custom router fee on output again.
     */
    function removeTokenPairFee(address tokenA, address tokenB)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        (address token0, address token1) = _sortTokens(tokenA, tokenB);
        if (!_tokenPairFees[token0][token1].isSet) return;
        --_tokenPairFeeCount;
        delete _tokenPairFees[token0][token1];
        emit TokenPairFeeRemoved(token0, token1);
    }

    /**
     * @dev Returns the pair fee for `tokenA` and `tokenB` in fee units.
     *      `isSet` is false when no pair fee is set.
     */
    function getTokenPairFee(address tokenA, address tokenB)
        external
        view
        returns (bool isSet, uint32 feeBps)
    {
        TokenPairFee memory pairFee = _getTokenPairFee(tokenA, tokenB);
        return (pairFee.isSet, pairFee.feeBps);
    }

    function _sortTokens(address tokenA, address tokenB)
        internal
        pure
        returns (address token0, address token1)
    {
        return tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
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
     * @notice Returns the router fee rates `calculateFee` applies for a client
     *         and a token pair
     * @dev Custom rates override the defaults per fee, and a pair fee applies
     *      as in `calculateFee`. A zero client resolves to tx.origin, as in
     *      `calculateFee`.
     * @param client The client address to check
     * @param tokenIn The swap's input token
     * @param tokenOut The swap's output token
     * @return routerFeeOnOutputBps Router fee on output in fee units
     * @return routerFeeOnClientFeeBps Router fee on client fee in fee units
     */
    function getEffectiveRouterFees(
        address client,
        address tokenIn,
        address tokenOut
    )
        external
        view
        returns (uint32 routerFeeOnOutputBps, uint32 routerFeeOnClientFeeBps)
    {
        address resolvedClient = _resolveClient(client);
        (routerFeeOnOutputBps, routerFeeOnClientFeeBps) =
            _getFeeInfo(resolvedClient);
        routerFeeOnOutputBps = _applyTokenPairFee(
            routerFeeOnOutputBps, resolvedClient, tokenIn, tokenOut
        );
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
     * @dev Sets the part of the positive slippage surplus the router
     *      captures. The rest of the surplus stays in the swap output.
     * @param slippageBps Router positive slippage in fee units (100_000_000 = the full surplus)
     */
    function setRouterPositiveSlippageBps(uint32 slippageBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (slippageBps > MAX_BPS) {
            revert FeeCalculator__RouterPositiveSlippageBpsTooHigh();
        }
        uint32 oldSlippageBps = _routerPositiveSlippageBps;
        _routerPositiveSlippageBps = slippageBps;
        emit RouterPositiveSlippageBpsUpdated(oldSlippageBps, slippageBps);
    }

    /**
     * @dev Returns the global router positive slippage bps in fee units
     */
    function getRouterPositiveSlippageBps() external view returns (uint32) {
        return _routerPositiveSlippageBps;
    }

    /**
     * @dev Sets a client's router positive slippage bps. It replaces the global capture
     *      bps for the client. Zero leaves the full surplus in the
     *      client's swap output.
     * @param client The client address to update
     * @param slippageBps Router positive slippage in fee units (100_000_000 = the full surplus)
     */
    function setCustomRouterPositiveSlippageBps(
        address client,
        uint32 slippageBps
    ) external onlyRole(ROUTER_FEE_SETTER_ROLE) {
        if (slippageBps > MAX_BPS) {
            revert FeeCalculator__RouterPositiveSlippageBpsTooHigh();
        }
        uint32 oldSlippageBps = _getRouterPositiveSlippageBps(client);
        _customRouterPositiveSlippageBps[client] =
            CustomRouterPositiveSlippageBps({
                isSet: true, slippageBps: slippageBps
            });
        emit CustomRouterPositiveSlippageBpsUpdated(
            client, oldSlippageBps, slippageBps
        );
    }

    /**
     * @dev Removes a client's router positive slippage bps, so the global router positive slippage bps
     *      applies to the client again
     */
    function removeCustomRouterPositiveSlippageBps(address client)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        delete _customRouterPositiveSlippageBps[client];
        emit CustomRouterPositiveSlippageBpsRemoved(client);
    }

    /**
     * @dev Returns a client's router positive slippage bps in fee units. `isSet` is false
     *      when the global router positive slippage bps applies to the client.
     */
    function getCustomRouterPositiveSlippageBps(address client)
        external
        view
        returns (bool isSet, uint32 slippageBps)
    {
        CustomRouterPositiveSlippageBps memory customSlippage =
            _customRouterPositiveSlippageBps[client];
        return (customSlippage.isSet, customSlippage.slippageBps);
    }

    /**
     * @dev Sets a client's share of the captured surplus. The router keeps
     *      the rest. Zero gives the full surplus to the router.
     * @param client The client address to update
     * @param shareBps Share in fee units (100_000_000 = the full surplus)
     */
    function setClientPositiveSlippageShare(address client, uint32 shareBps)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
    {
        if (shareBps > MAX_BPS) revert FeeCalculator__ShareTooHigh();
        uint32 oldShareBps = _clientPositiveSlippageShareBps[client];
        _clientPositiveSlippageShareBps[client] = shareBps;
        emit ClientPositiveSlippageShareUpdated(client, oldShareBps, shareBps);
    }

    /**
     * @dev Returns a client's share of the captured surplus in fee units
     */
    function getClientPositiveSlippageShare(address client)
        external
        view
        returns (uint32)
    {
        return _clientPositiveSlippageShareBps[client];
    }
}
