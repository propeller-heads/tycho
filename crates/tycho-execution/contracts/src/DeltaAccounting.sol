// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {
    ReentrancyGuard
} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Pausable} from "@openzeppelin/contracts/utils/Pausable.sol";

error DeltaAccounting__UnexpectedNonZeroCount(uint256 nonZeroCount);

/**
 * @title DeltaAccounting - per-token transient balance deltas of the router
 * @dev A delta is positive when tokens arrive at the router and negative when
 * they leave. Every delta must be zero when a swap ends, so tokens can neither
 * stay in the router nor leave it beyond what the swap brought in.
 */
abstract contract DeltaAccounting is ReentrancyGuard, Pausable {
    // One directive per slot type: Slither 0.11.6 can fail to analyse a
    // contract that uses `using TransientSlot for *`.
    using TransientSlot for bytes32;
    using TransientSlot for TransientSlot.Int256Slot;
    using TransientSlot for TransientSlot.Uint256Slot;

    TransientSlot.Uint256Slot private constant _NON_ZERO_DELTA_COUNT_SLOT =
        TransientSlot.Uint256Slot
            .wrap(keccak256("DeltaAccounting#NON_ZERO_DELTA_COUNT_SLOT"));

    /**
     * @dev Returns the transient storage slot of the delta of `token`
     */
    function _getDeltaSlot(address token) private pure returns (bytes32 slot) {
        slot = keccak256(abi.encodePacked(token, "DeltaAccounting#DELTA"));
    }

    /**
     * @dev Returns the current delta of `token`
     */
    function _getDelta(address token) internal view returns (int256 delta) {
        delta = _getDeltaSlot(token).asInt256().tload();
    }

    /**
     * @dev Returns the number of tokens with a non-zero delta
     */
    function _getNonZeroDeltaCount() internal view returns (uint256 count) {
        count = _NON_ZERO_DELTA_COUNT_SLOT.tload();
    }

    /**
     * @dev Adds `deltaChange` to the delta of `token`
     * @param token The token to update
     * @param deltaChange The change to apply (positive to credit, negative to debit)
     */
    function _updateDeltaAccounting(address token, int256 deltaChange)
        internal
        virtual
    {
        if (deltaChange == 0) return;

        int256 oldDelta = _getDelta(token);
        int256 newDelta = oldDelta + deltaChange;

        if (oldDelta != 0 && newDelta == 0) {
            _NON_ZERO_DELTA_COUNT_SLOT.tstore(_getNonZeroDeltaCount() - 1);
        } else if (oldDelta == 0 && newDelta != 0) {
            _NON_ZERO_DELTA_COUNT_SLOT.tstore(_getNonZeroDeltaCount() + 1);
        }

        _getDeltaSlot(token).asInt256().tstore(newDelta);
    }

    /**
     * @dev Reverts unless every delta is zero
     */
    function _finalizeBalances() internal view {
        uint256 nonZeroCount = _getNonZeroDeltaCount();
        if (nonZeroCount > 0) {
            revert DeltaAccounting__UnexpectedNonZeroCount(nonZeroCount);
        }
    }
}
