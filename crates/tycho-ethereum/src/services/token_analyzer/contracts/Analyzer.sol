// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
}

interface IForwarder {
    function forwardTransfer(address token, address to, uint256 amount) external returns (bool);
    function forwardApprove(address token, address spender, uint256 amount) external returns (bool);
}

/// @title Token Analyzer
/// @notice Injected at a token holder's address via eth_call state override. Performs a full
/// sell-fee analysis in a single call using four transfer legs:
///
///   Leg 1 (buy):        holder → settlement           — detects buy fees
///   Leg 2 (roundtrip):  settlement → holder           — detects fees even to whitelisted LP
///   Leg 3 (retransfer): holder → settlement            — returns tokens for the sell test
///   Leg 4 (sell):       settlement → recipient        — detects sell-only fees
///
/// If the roundtrip (legs 1-2) passes but the sell test (legs 3-4) fails, the token has a
/// sell-only fee: it charges fees only when transferring to non-whitelisted addresses.
///
/// @dev Inbound transfers (legs 1 and 3) use low-level calls so that tokens with non-standard
/// transfer() implementations (e.g. USDT, which omits the bool return value) are handled
/// correctly. balanceOf and approve are called via the typed interface.
contract Analyzer {
    /// @notice Simulate a full ERC20 transfer analysis in four legs.
    /// @param token        The ERC20 token to analyze.
    /// @param amount       The amount to transfer in each buy leg.
    /// @param settlement   Intermediary address (injected with Forwarder bytecode).
    /// @param recipient    Final arbitrary recipient for the sell-only fee test.
    /// @return transferInOk       Whether leg 1 (holder → settlement) succeeded.
    /// @return roundtripOutOk     Whether leg 2 (settlement → holder) succeeded.
    /// @return transferOutOk      Whether leg 4 (settlement → recipient) succeeded.
    /// @return approvalOk         Whether forwardApprove(MAX_UINT256) succeeded.
    /// @return balanceBeforeIn    Settlement balance before leg 1.
    /// @return balanceAfterIn     Settlement balance after leg 1.
    /// @return balanceAfterRoundtrip  Settlement balance after leg 2.
    /// @return holderBefore       Holder balance before leg 1.
    /// @return holderAfter        Holder balance after leg 2.
    /// @return balanceAfterOut    Settlement balance after leg 4.
    /// @return recipientBefore    Recipient balance before leg 4.
    /// @return recipientAfter     Recipient balance after leg 4.
    /// @return gasIn              Gas consumed by leg 1.
    /// @return roundtripGasOut    Gas consumed by leg 2.
    /// @return gasOut             Gas consumed by leg 4.
    function analyze(
        address token,
        uint256 amount,
        address settlement,
        address recipient
    ) external returns (
        bool transferInOk,
        bool roundtripOutOk,
        bool transferOutOk,
        bool approvalOk,
        uint256 balanceBeforeIn,
        uint256 balanceAfterIn,
        uint256 balanceAfterRoundtrip,
        uint256 holderBefore,
        uint256 holderAfter,
        uint256 balanceAfterOut,
        uint256 recipientBefore,
        uint256 recipientAfter,
        uint256 gasIn,
        uint256 roundtripGasOut,
        uint256 gasOut
    ) {
        IERC20 erc20 = IERC20(token);

        balanceBeforeIn = erc20.balanceOf(settlement);
        holderBefore = erc20.balanceOf(address(this));
        recipientBefore = erc20.balanceOf(recipient);

        // === Leg 1: Buy — holder → settlement ===
        uint256 g1 = gasleft();
        {
            (bool ok, bytes memory data) = token.call(
                abi.encodeWithSelector(0xa9059cbb, settlement, amount)
            );
            transferInOk = ok && (data.length == 0 || abi.decode(data, (bool)));
        }
        gasIn = g1 - gasleft();

        if (!transferInOk) {
            return (false, false, false, false, balanceBeforeIn, 0, 0, holderBefore, 0, 0, recipientBefore, 0, gasIn, 0, 0);
        }

        balanceAfterIn = erc20.balanceOf(settlement);

        // Guard: a token that returns true but reduces settlement balance is pathological.
        if (balanceAfterIn < balanceBeforeIn) {
            return (false, false, false, false, balanceBeforeIn, balanceAfterIn, 0, holderBefore, 0, 0, recipientBefore, 0, gasIn, 0, 0);
        }

        uint256 received = balanceAfterIn - balanceBeforeIn;

        // === Leg 2: Roundtrip — settlement → holder ===
        uint256 g2 = gasleft();
        try IForwarder(settlement).forwardTransfer(token, address(this), received) returns (bool s) {
            roundtripOutOk = s;
        } catch {
            roundtripOutOk = false;
        }
        roundtripGasOut = g2 - gasleft();

        holderAfter = erc20.balanceOf(address(this));
        balanceAfterRoundtrip = erc20.balanceOf(settlement);

        if (!roundtripOutOk) {
            return (true, false, false, false, balanceBeforeIn, balanceAfterIn, balanceAfterRoundtrip, holderBefore, holderAfter, 0, recipientBefore, 0, gasIn, roundtripGasOut, 0);
        }

        // === Leg 3: Retransfer — holder → settlement (returns the received tokens for the sell test) ===
        // holderAfter = holderBefore - amount + roundtripReceived
        // ⟹ roundtripReceived = holderAfter + amount - holderBefore
        // Safe: holderAfter >= holderBefore - amount (holder only gains from roundtrip),
        // so holderAfter + amount >= holderBefore (no underflow).
        uint256 roundtripReceived = holderAfter + amount - holderBefore;
        {
            (bool ok, bytes memory data) = token.call(
                abi.encodeWithSelector(0xa9059cbb, settlement, roundtripReceived)
            );
            // Intentionally not checked: if this fails, leg 4 will also fail (settlement
            // has no tokens), and transferOutOk will correctly capture the outcome.
            (ok, data);
        }

        // === Leg 4: Sell — settlement → recipient (arbitrary) ===
        uint256 g3 = gasleft();
        try IForwarder(settlement).forwardTransfer(token, recipient, roundtripReceived) returns (bool s) {
            transferOutOk = s;
        } catch {
            transferOutOk = false;
        }
        gasOut = g3 - gasleft();

        if (!transferOutOk) {
            balanceAfterOut = erc20.balanceOf(settlement);
            return (true, true, false, false, balanceBeforeIn, balanceAfterIn, balanceAfterRoundtrip, holderBefore, holderAfter, balanceAfterOut, recipientBefore, 0, gasIn, roundtripGasOut, gasOut);
        }

        balanceAfterOut = erc20.balanceOf(settlement);
        recipientAfter = erc20.balanceOf(recipient);

        // Test that settlement can approve (some tokens block approvals from contracts).
        try IForwarder(settlement).forwardApprove(token, recipient, type(uint256).max) returns (bool s) {
            approvalOk = s;
        } catch {
            approvalOk = false;
        }
    }
}
