// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.0;

interface IUniswapV2Pair {
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IWETH {
    function deposit() external payable;
}

/// @title Swap Runner
/// @notice Injected at a router address via eth_simulateV1 state override; never deployed. Trades
/// against a Uniswap V2 style pair the way a router does, so the token sees a real swap: a wallet
/// as `tx.origin`, the pair as the sender of the buy, the pair locked during the transfer, and a
/// `Swap` event. `buy` wraps the ETH sent, swaps it for the token and forwards the output to a
/// receiver in the same transaction, as a router that forwards pool output does. `sell` takes the
/// caller's whole balance with `transferFrom` and swaps it back.
/// @dev Token calls are low-level so that tokens without a bool return value and reverting tokens
/// are measured instead of reverting the transaction.
contract SwapRunner {
    struct Buy {
        bool swapOk;
        uint256 expected;
        uint256 bought;
        bool forwardOk;
        uint256 forwardSent;
        uint256 forwardReceived;
        uint256 forwardGas;
        bytes swapRevert;
        bytes forwardRevert;
    }

    struct Sell {
        bool transferOk;
        uint256 sent;
        uint256 arrived;
        bool swapOk;
        uint256 wethOut;
        uint256 gas;
        bytes transferRevert;
        bytes swapRevert;
    }

    function buy(address token, address pair, address weth, address receiver)
        external
        payable
        returns (Buy memory r)
    {
        IWETH(weth).deposit{value: msg.value}();
        bool tokenIs0 = IUniswapV2Pair(pair).token0() == token;
        (uint256 reserveIn, uint256 reserveOut) = reserves(pair, tokenIs0);
        r.expected = amountOut(msg.value, reserveIn, reserveOut);
        (bool sent,) = weth.call(abi.encodeWithSelector(0xa9059cbb, pair, msg.value));
        require(sent, "WETH transfer failed");

        uint256 before = balanceOf(token, address(this));
        (r.swapOk, r.swapRevert) = pair.call(
            abi.encodeWithSelector(
                IUniswapV2Pair.swap.selector,
                tokenIs0 ? r.expected : 0,
                tokenIs0 ? 0 : r.expected,
                address(this),
                ""
            )
        );
        if (!r.swapOk) {
            return r;
        }
        uint256 held = balanceOf(token, address(this));
        r.bought = held > before ? held - before : 0;
        if (r.bought == 0) {
            return r;
        }

        r.forwardSent = r.bought;
        uint256 receiverBefore = balanceOf(token, receiver);
        uint256 gas = gasleft();
        (r.forwardOk, r.forwardRevert) = call(token, abi.encodeWithSelector(0xa9059cbb, receiver, r.bought));
        r.forwardGas = gas - gasleft();
        uint256 receiverAfter = balanceOf(token, receiver);
        r.forwardReceived = receiverAfter > receiverBefore ? receiverAfter - receiverBefore : 0;
    }

    function sell(address token, address pair, address weth) external returns (Sell memory r) {
        r.sent = balanceOf(token, msg.sender);
        uint256 pairBefore = balanceOf(token, pair);
        uint256 gas = gasleft();
        (r.transferOk, r.transferRevert) =
            call(token, abi.encodeWithSelector(0x23b872dd, msg.sender, pair, r.sent));
        r.gas = gas - gasleft();
        if (!r.transferOk) {
            return r;
        }
        uint256 pairAfter = balanceOf(token, pair);
        r.arrived = pairAfter > pairBefore ? pairAfter - pairBefore : 0;

        bool tokenIs0 = IUniswapV2Pair(pair).token0() == token;
        (uint256 reserveToken, uint256 reserveWeth) = reserves(pair, !tokenIs0);
        // What the pair holds beyond its reserves is the input, as a router supporting
        // fee-on-transfer tokens computes it.
        uint256 input = pairAfter > reserveToken ? pairAfter - reserveToken : 0;
        uint256 out = amountOut(input, reserveToken, reserveWeth);
        uint256 wethBefore = balanceOf(weth, msg.sender);
        (r.swapOk, r.swapRevert) = pair.call(
            abi.encodeWithSelector(
                IUniswapV2Pair.swap.selector, tokenIs0 ? 0 : out, tokenIs0 ? out : 0, msg.sender, ""
            )
        );
        uint256 wethAfter = balanceOf(weth, msg.sender);
        r.wethOut = wethAfter > wethBefore ? wethAfter - wethBefore : 0;
    }

    /// Reserves as (in, out) for a trade whose output is token0 when `outIs0`.
    function reserves(address pair, bool outIs0) private view returns (uint256, uint256) {
        (uint112 r0, uint112 r1,) = IUniswapV2Pair(pair).getReserves();
        return outIs0 ? (uint256(r1), uint256(r0)) : (uint256(r0), uint256(r1));
    }

    /// The constant-product output for `amountIn` after a 5% fee. Pairs charge different fees,
    /// and a pair reverts when asked for more than its fee allows; it sends exactly the amount
    /// asked for, so asking for less does not change what the token's fee is measured against.
    function amountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut)
        private
        pure
        returns (uint256)
    {
        if (amountIn == 0 || reserveIn == 0 || reserveOut == 0) {
            return 0;
        }
        uint256 withFee = amountIn * 950;
        return (withFee * reserveOut) / (reserveIn * 1000 + withFee);
    }

    function call(address token, bytes memory data) private returns (bool ok, bytes memory revertData) {
        bytes memory out;
        (ok, out) = token.call(data);
        if (!ok) {
            return (false, out);
        }
        ok = out.length == 0 || (out.length >= 32 && abi.decode(out, (bool)));
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
