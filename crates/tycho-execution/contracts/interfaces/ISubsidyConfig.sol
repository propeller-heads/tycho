// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/// @notice Subsidy settings that `TychoSubsidizingExecutor` reads.
interface ISubsidyConfig {
    /// @dev A zero wallet or signer disables subsidies. The wallet pays from
    ///      its vault balance. The rate caps a subsidy, in fee units of the
    ///      hop input.
    function getSubsidyConfig()
        external
        view
        returns (
            address subsidyWallet,
            address subsidySigner,
            uint32 subsidyBps
        );
}
