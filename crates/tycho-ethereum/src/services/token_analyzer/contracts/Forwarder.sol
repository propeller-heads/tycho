// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Token Forwarder
/// @notice Proxies ERC20 transfer and approve calls on behalf of the address at which this
/// contract is injected, allowing the Analyzer to simulate outbound transfers.
/// @dev Both forwardTransfer and forwardApprove use low-level calls so that tokens which omit
/// the bool return value (e.g. USDT, which omits it from both transfer() and approve()) are
/// handled correctly.
contract Forwarder {
    /// @notice Forward an ERC20 transfer from this address to a recipient.
    function forwardTransfer(address token, address to, uint256 amount) external returns (bool) {
        (bool ok, bytes memory data) = token.call(
            abi.encodeWithSelector(0xa9059cbb, to, amount)
        );
        return ok && (data.length == 0 || abi.decode(data, (bool)));
    }

    /// @notice Forward an ERC20 approve from this address to a spender.
    function forwardApprove(address token, address spender, uint256 amount) external returns (bool) {
        (bool ok, bytes memory data) = token.call(
            abi.encodeWithSelector(0x095ea7b3, spender, amount)
        );
        return ok && (data.length == 0 || abi.decode(data, (bool)));
    }
}
