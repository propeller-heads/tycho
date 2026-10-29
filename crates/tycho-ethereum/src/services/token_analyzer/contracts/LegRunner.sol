// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.0;

/// @title Leg Runner
/// @notice Injected at the simulated trader's address via eth_simulateV1 state override. The pool
/// sends the trader tokens in a separate call (the buy); `legs` then forwards half to a wallet and
/// sells the rest back to the pool, measuring what arrived at each step. Never deployed.
/// @dev The buy moves tokens out of the pool without a swap, so a Uniswap V2 style pool's reserves
/// no longer match its balances, and a token that swaps its fees through that pool during the
/// transfer would underflow there. `legs` first calls `sync()` on the pool, as a swap would have
/// updated the reserves; pools without it ignore the call.
/// @dev Token calls are low-level so that tokens without a bool return value (USDT) and reverting
/// tokens are measured instead of reverting the whole call. Balances are read the same way and
/// read as zero when `balanceOf` reverts.
contract LegRunner {
    /// Trader balance before the buy, recorded by `record`. Some tokens report a balance for
    /// addresses that never received any, so the buy is measured as a difference.
    uint256 private recorded;

    struct Result {
        uint256 bought;
        bool transferOk;
        uint256 transferSent;
        uint256 transferReceived;
        uint256 transferGas;
        bool approveOk;
        bool sellOk;
        uint256 sellSent;
        uint256 sellArrived;
        uint256 sellGas;
        /// Change of the wallet's balance during the sell, which the wallet takes no part in.
        int256 walletDrift;
        bytes transferRevert;
        bytes sellRevert;
    }

    function record(address token) external {
        recorded = balanceOf(token, address(this));
    }

    function legs(address token, address pool, address wallet) external returns (Result memory r) {
        uint256 balance = balanceOf(token, address(this));
        r.bought = balance > recorded ? balance - recorded : 0;
        if (r.bought == 0) {
            return r;
        }
        // sync()
        pool.call(abi.encodeWithSelector(0xfff6cae9));

        r.transferSent = r.bought / 2;
        uint256 walletBefore = balanceOf(token, wallet);
        uint256 gas = gasleft();
        (r.transferOk, r.transferRevert) = transfer(token, wallet, r.transferSent);
        r.transferGas = gas - gasleft();
        uint256 walletAfter = balanceOf(token, wallet);
        r.transferReceived = walletAfter > walletBefore ? walletAfter - walletBefore : 0;

        (r.approveOk,) = token.call(abi.encodeWithSelector(0x095ea7b3, pool, type(uint256).max));

        r.sellSent = r.transferOk ? r.bought - r.transferSent : r.bought;
        uint256 held = balanceOf(token, address(this));
        if (r.sellSent > held) {
            r.sellSent = held;
        }
        uint256 poolBefore = balanceOf(token, pool);
        gas = gasleft();
        (r.sellOk, r.sellRevert) = transfer(token, pool, r.sellSent);
        r.sellGas = gas - gasleft();
        uint256 poolAfter = balanceOf(token, pool);
        r.sellArrived = poolAfter > poolBefore ? poolAfter - poolBefore : 0;
        r.walletDrift = int256(balanceOf(token, wallet)) - int256(walletAfter);
    }

    function transfer(address token, address to, uint256 amount)
        private
        returns (bool ok, bytes memory revertData)
    {
        bytes memory data;
        (ok, data) = token.call(abi.encodeWithSelector(0xa9059cbb, to, amount));
        if (!ok) {
            return (false, data);
        }
        ok = data.length == 0 || (data.length >= 32 && abi.decode(data, (bool)));
    }

    function balanceOf(address token, address holder) private view returns (uint256) {
        (bool ok, bytes memory data) =
            token.staticcall(abi.encodeWithSelector(0x70a08231, holder));
        if (!ok || data.length < 32) {
            return 0;
        }
        return abi.decode(data, (uint256));
    }
}
