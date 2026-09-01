// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IUniswapV3Pool
} from "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import {TransferManager} from "../TransferManager.sol";

error SelfAccountingFallbackExecutor__InvalidDataLength();
error SelfAccountingFallbackExecutor__InsufficientRouterBalance(
    uint256 available, uint256 required
);
error SelfAccountingFallbackExecutor__UnauthorizedCallback(address caller);

/// @title SelfAccountingFallbackExecutor (EXPERIMENTAL PROTOTYPE)
/// @notice Tries a Uniswap V3-style pool first and, if that pool call
/// reverts, executes the same hop on a Uniswap V2-style pair instead.
/// The two legs use different transfer scenarios (callback vs. direct
/// transfer), a pairing the Dispatcher cannot express because it fixes one
/// `TransferType` per hop. This executor therefore declares
/// `TransferType.None` for both the swap and the callback and performs the
/// input transfer plus the Vault's transient delta accounting itself.
///
/// @dev EXPERIMENT ONLY - DO NOT DEPLOY. This contract deliberately violates
/// the executor rules in crates/tycho-execution/CLAUDE.md ("never call
/// ERC20.transfer in an executor", "no accounting in executors"): it calls
/// `IERC20.transfer` from the router's balance and writes the Vault's
/// transient delta slots directly. It exists to evaluate an executor-level
/// protocol fallback that needs no Dispatcher or TychoRouterV3 change.
///
/// Replicated router internals (must match the deployed router byte-for-byte;
/// verified against Vault.sol and Dispatcher.sol):
/// - Vault per-token delta slot:
///   `uint256(keccak256(abi.encodePacked(token, "TychoVault#DELTA")))`
/// - `Vault._NON_ZERO_DELTA_COUNT_SLOT`
/// - `Dispatcher._SWAP_INPUT_AMOUNT_SLOT` / `_SWAP_INPUT_TOKEN_SLOT` (read in
///   the callback to learn the input token and amount; note the amount slot
///   constant in Dispatcher.sol does NOT equal the keccak256 of its comment
///   string - the literal is copied, not recomputed)
///
/// Why the primary leg must be callback-based: no tokens move before the
/// pool call, so a revert inside `IUniswapV3Pool.swap` rolls back everything
/// including the input transfer made in the callback. A direct-transfer
/// primary could not be rolled back from this call frame.
///
/// Scope limit: router-held input only (sequential middle hops and
/// vault-funded swaps). First swaps funded from a user wallet
/// (transferFrom/Permit2) are NOT supported - with `TransferType.None` the
/// Dispatcher never pulls user funds, so `swap` reverts with
/// `SelfAccountingFallbackExecutor__InsufficientRouterBalance`.
contract SelfAccountingFallbackExecutor is IExecutor, ICallback {
    using SafeERC20 for IERC20;

    uint160 private constant _MIN_SQRT_RATIO = 4295128739;
    uint160 private constant _MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;
    uint256 private constant _V2_FEE_BPS = 30;

    // keccak256("SelfAccountingFallbackExecutor#PRIMARY_POOL_SLOT")
    uint256 private constant _PRIMARY_POOL_SLOT =
        0xebad11c471ee92e42360e7292a495cb5df9d1beb18bcda045d008f6af018eaaf;
    // Copied from Vault.sol (comment there: keccak256("TychoVault#NON_ZERO_DELTA_COUNT_SLOT"))
    uint256 private constant _NON_ZERO_DELTA_COUNT_SLOT =
        0xee3c9c434505299f2450d3624302a27b8a6978e973825330bc744ba925eec199;
    // Copied from Dispatcher.sol - literal value, NOT keccak256 of the name
    uint256 private constant _SWAP_INPUT_AMOUNT_SLOT =
        0xce9e2e8e50d57f2d688020ea7ab16e2039bcf4dc7175eba827e178586597bb39;
    // Copied from Dispatcher.sol (comment there: keccak256("Dispatcher#SWAP_INPUT_TOKEN_SLOT"))
    uint256 private constant _SWAP_INPUT_TOKEN_SLOT =
        0x0c22c14aba48b0e26e3b58475c66f358c352532122e537c32e8184c0159e6e10;

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        // Input tokens must sit at the router before this hop runs.
        return msg.sender;
    }

    function getTransferData(bytes calldata data)
        external
        pure
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (tokenIn, tokenOut,,,) = _decodeData(data);
        // None: the Dispatcher performs no input transfer. This executor
        // moves the input itself (callback for the primary leg, direct
        // transfer for the fallback leg).
        transferType = TransferManager.TransferType.None;
        receiver = address(0);
        outputToRouter = false;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address tokenIn,
            address tokenOut,
            address primaryPool,
            bool zeroForOne,
            address fallbackPool
        ) = _decodeData(data);

        // Runs via delegatecall: address(this) is the router. Only
        // router-held input is supported (see contract natspec).
        uint256 routerBalance = IERC20(tokenIn).balanceOf(address(this));
        if (routerBalance < amountIn) {
            revert SelfAccountingFallbackExecutor__InsufficientRouterBalance(
                routerBalance, amountIn
            );
        }

        // Remember the primary pool so handleCallback can verify the caller.
        // slither-disable-next-line assembly
        assembly {
            tstore(_PRIMARY_POOL_SLOT, primaryPool)
        }

        // Primary leg. On success the pool's callback (routed through the
        // router's fallback into handleCallback below) has already paid the
        // input and debited the delta. On revert everything - including that
        // callback transfer - is rolled back, leaving the input untouched at
        // the router for the fallback leg.
        try IUniswapV3Pool(primaryPool)
            .swap(
                receiver,
                zeroForOne,
                int256(amountIn),
                zeroForOne ? _MIN_SQRT_RATIO + 1 : _MAX_SQRT_RATIO - 1,
                ""
            ) returns (
            int256, int256
        ) {
            // slither-disable-next-line assembly
            assembly {
                tstore(_PRIMARY_POOL_SLOT, 0)
            }
            return;
        } catch {
            // slither-disable-next-line assembly
            assembly {
                tstore(_PRIMARY_POOL_SLOT, 0)
            }
        }

        // Fallback leg: Uniswap V2-style direct-transfer swap. Last resort -
        // if this reverts the whole transaction reverts, so no rollback is
        // needed.
        _debitRouter(tokenIn, amountIn);
        IERC20(tokenIn).safeTransfer(fallbackPool, amountIn);
        _v2Swap(
            IUniswapV2Pair(fallbackPool), amountIn, tokenIn < tokenOut, receiver
        );
    }

    /// @dev Pays the primary pool from the router's balance and mirrors the
    /// delta debit the TransferManager would have done for a
    /// `TransferType.Transfer` hop. Runs via delegatecall in the router's
    /// storage context; msg.sender is the pool that called the router's
    /// fallback.
    function handleCallback(
        bytes calldata /* msgData */
    )
        external
        returns (bytes memory)
    {
        address primaryPool;
        address tokenIn;
        uint256 amount;
        // slither-disable-next-line assembly
        assembly {
            primaryPool := tload(_PRIMARY_POOL_SLOT)
            tokenIn := tload(_SWAP_INPUT_TOKEN_SLOT)
            amount := tload(_SWAP_INPUT_AMOUNT_SLOT)
        }

        if (msg.sender != primaryPool || primaryPool == address(0)) {
            revert SelfAccountingFallbackExecutor__UnauthorizedCallback(msg.sender);
        }

        _debitRouter(tokenIn, amount);
        IERC20(tokenIn).safeTransfer(primaryPool, amount);
        return "";
    }

    function getCallbackTransferData(
        bytes calldata, /* data */
        address, /* tokenIn */
        address /* caller */
    )
        external
        pure
        returns (TransferManager.TransferType transferType, address receiver)
    {
        // None: handleCallback performs the transfer itself.
        transferType = TransferManager.TransferType.None;
        receiver = address(0);
    }

    /// @dev Replica of Vault._updateDeltaAccounting(token, -amount),
    /// including the nonZeroDeltaCount transitions, written to the same
    /// transient slots so the router's _finalizeBalances still passes.
    function _debitRouter(address token, uint256 amount) private {
        // slither-disable-next-line incorrect-equality
        if (amount == 0) return;

        // Replica of Vault._getDeltaSlot
        uint256 slot =
            uint256(keccak256(abi.encodePacked(token, "TychoVault#DELTA")));

        int256 oldDelta;
        uint256 nonZeroCount;
        // slither-disable-next-line assembly
        assembly {
            oldDelta := tload(slot)
            nonZeroCount := tload(_NON_ZERO_DELTA_COUNT_SLOT)
        }

        int256 newDelta = oldDelta - int256(amount);
        // slither-disable-next-line incorrect-equality
        if (oldDelta != 0 && newDelta == 0) {
            nonZeroCount -= 1;
            // slither-disable-next-line incorrect-equality
        } else if (oldDelta == 0 && newDelta != 0) {
            nonZeroCount += 1;
        }

        // slither-disable-next-line assembly
        assembly {
            tstore(_NON_ZERO_DELTA_COUNT_SLOT, nonZeroCount)
            tstore(slot, newDelta)
        }
    }

    /// @dev Constant-product output math and pool call, mirroring
    /// UniswapV2Executor with the canonical 30 bps fee.
    function _v2Swap(
        IUniswapV2Pair pool,
        uint256 amountIn,
        bool zeroForOne,
        address receiver
    ) private {
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pool.getReserves();
        uint112 reserveIn = zeroForOne ? reserve0 : reserve1;
        uint112 reserveOut = zeroForOne ? reserve1 : reserve0;

        require(reserveIn > 0 && reserveOut > 0, "L");
        uint256 amountInWithFee = amountIn * (10000 - _V2_FEE_BPS);
        uint256 numerator = amountInWithFee * uint256(reserveOut);
        uint256 denominator = (uint256(reserveIn) * 10000) + amountInWithFee;
        uint256 amountOut = numerator / denominator;

        if (zeroForOne) {
            pool.swap(0, amountOut, receiver, "");
        } else {
            pool.swap(amountOut, 0, receiver, "");
        }
    }

    function _decodeData(bytes calldata data)
        private
        pure
        returns (
            address tokenIn,
            address tokenOut,
            address primaryPool,
            bool zeroForOne,
            address fallbackPool
        )
    {
        if (data.length != 81) {
            revert SelfAccountingFallbackExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        primaryPool = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[60]) > 0;
        fallbackPool = address(bytes20(data[61:81]));
    }
}
