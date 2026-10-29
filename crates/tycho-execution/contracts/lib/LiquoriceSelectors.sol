// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/// @dev The settlement entrypoints a Liquorice quote may call. Liquorice redeploys settlement
///      versions rather than upgrading in place, and `settle`'s selector moves with its `Order`
///      tuple.
bytes4 constant LIQUORICE_SETTLE_SINGLE_SELECTOR = 0x9935c868;
bytes4 constant LIQUORICE_SETTLE_SELECTOR = 0x053b4100;
