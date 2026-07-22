// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";

/**
 * @notice Per-client custom fee configuration
 * @dev All fields pack into a single storage slot (10 bytes total).
 *      Fee values use 8-decimal precision: 1 unit = 0.0001 BPS = 0.000001%.
 *      100% = 100_000_000 units.
 */
struct CustomFees {
    bool hasCustomFeeOnOutput; // 1 byte
    uint32 feeBpsOnOutput; // 4 bytes
    bool hasCustomFeeOnClientFee; // 1 byte
    uint32 feeBpsOnClientFee; // 4 bytes
}

interface IFeeCalculator {
    /**
     * @notice Calculates all fees and slippage surplus from swap output
     * @dev Called from TychoRouter. Does not perform any accounting.
     *      Handles both regular fees and positive slippage surplus
     *      in a single call. The full surplus goes to the router.
     *      Router fee parameters are retrieved from contract storage
     *      based on the client address; client fee parameters are
     *      passed as function arguments.
     * @param feeInput Struct containing all fee calculation inputs
     * @return feeRecipients Array of (address, feeAmount) tuples for
     *         fee distribution. Returns [] when there is nothing to
     *         capture (no fees, no surplus, or toggle off).
     */
    function calculateFee(FeeInput memory feeInput)
        external
        view
        returns (FeeRecipient[] memory feeRecipients);

    /**
     * @notice Whether the router must receive swap output before forwarding
     * @dev Covers: slippage enabled, fees > 0, or any future condition.
     * @param clientFeeBps Client fee in fee units (100_000_000 = 100%)
     * @param client The client address to check
     * @return True if funds must pass through the router after the
     *         final swap instead of going directly to the receiver
     */
    function mustInterceptOutput(uint32 clientFeeBps, address client)
        external
        view
        returns (bool);

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
        returns (address[] memory clients, CustomFees[] memory fees);
}
