// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {
    IPancakeswapInfinityVault,
    IPancakeswapInfinityCLPoolManager,
    IPancakeswapInfinityBinPoolManager,
    InfinityPoolKey
} from "@interfaces/IPancakeswapInfinity.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {TransferManager} from "../TransferManager.sol";
import {TickMath} from "@uniswap/v4-core/src/libraries/TickMath.sol";
import {SafeCast} from "@uniswap/v4-core/src/libraries/SafeCast.sol";

error PancakeswapInfinityExecutor__InvalidDataLength();
error PancakeswapInfinityExecutor__NotVault();
error PancakeswapInfinityExecutor__UnexpectedCallback(bytes4 selector);
error PancakeswapInfinityExecutor__UnknownPoolType(uint8 poolType);
error PancakeswapInfinityExecutor__ZeroAddress();
/// @dev The Vault owes nothing on this currency, so it must be settled rather than taken.
error PancakeswapInfinityExecutor__DeltaNegative(address currency);

/// @notice Executor for PancakeSwap Infinity CL and Bin pools.
//
// Uniswap v4 fork (ref: UniswapV4Executor.sol), but funds and the lock live in a separate Vault:
// settlement hits the Vault, the swap hits one of two managers picked by a pool-type byte.
//
// Swap data, 97 bytes packed:
//   [tokenIn 20][tokenOut 20][zeroForOne 1][poolType 1][fee 3][parameters 32][hooks 20]
//
// CANONICAL zeroForOne, cited by swap_encoder/pancakeswap_infinity.rs. Only contract between the
// two sides. A mismatch misorders the key, so the pool id addresses nothing and the swap reverts
// PoolNotInitialized:
//   zeroForOne == (infinityCurrency(tokenIn) < infinityCurrency(tokenOut))
// Compared after ETH_ADDRESS -> address(0), so native is always currency0. Never recomputed here.
//
// Dispatcher delegatecalls both entry points: address(this) is the ROUTER, which is the Vault's
// locker and what currencyDelta looks up.
//
// Lock payload: abi.encodePacked(amountIn, receiver, data) = 149 bytes, packed not abi.encode
// since _decodeData needs calldata. Callback slices data[68:217]; the ABI tail pads 149 to 160.
contract PancakeswapInfinityExecutor is IExecutor, ICallback {
    using SafeCast for uint256;

    /// @dev Packed swap data. Layout above.
    uint256 private constant _DATA_LENGTH = 97;
    /// @dev Lock payload: 32 + 20 + _DATA_LENGTH.
    // Used only as a calldata slice bound, which Slither does not count as a use.
    // slither-disable-next-line unused-state
    uint256 private constant _PAYLOAD_LENGTH = 149;
    /// @dev lockAcquired header: 4 + 32 + 32.
    // Same slice-bound false positive as _PAYLOAD_LENGTH.
    // slither-disable-next-line unused-state
    uint256 private constant _CALLBACK_HEADER = 68;
    bytes4 private constant _LOCK_ACQUIRED =
        bytes4(keccak256("lockAcquired(bytes)"));
    uint8 private constant _POOL_TYPE_CL = 0;
    uint8 private constant _POOL_TYPE_BIN = 1;

    IPancakeswapInfinityVault public immutable vault;
    address public immutable clPoolManager;
    address public immutable binPoolManager;

    constructor(
        IPancakeswapInfinityVault vault_,
        address clPoolManager_,
        address binPoolManager_
    ) {
        if (
            address(vault_) == address(0) || clPoolManager_ == address(0)
                || binPoolManager_ == address(0)
        ) {
            revert PancakeswapInfinityExecutor__ZeroAddress();
        }
        vault = vault_;
        clPoolManager = clPoolManager_;
        binPoolManager = binPoolManager_;
    }

    /// @dev Input reaches the Vault inside the callback, so a preceding hop must send its
    /// output to the router first.
    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    modifier vaultOnly() {
        if (msg.sender != address(vault)) {
            revert PancakeswapInfinityExecutor__NotVault();
        }
        _;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn,,,,,,) = _decodeData(data);
        // Native pays by value in the callback, no snapshot needed. ERC20 must sync before
        // lock: settle credits balanceNow - reservesBefore, and the token only lands once the
        // callback is running.
        // https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/Vault.sol#L209-L224
        if (tokenIn != ETH_ADDRESS) {
            vault.sync(tokenIn);
        }
        // Vault re-enters the router with lockAcquired -> handleCallback.
        // slither-disable-next-line unused-return
        vault.lock(abi.encodePacked(amountIn, receiver, data));
    }

    function verifyCallback(
        bytes calldata /* data */
    )
        public
        view
        vaultOnly
    {}

    /// @dev Raw lockAcquired(bytes) calldata from the router's fallback: 4 selector + 32 offset
    /// + 32 length, then our payload.
    function handleCallback(bytes calldata data)
        external
        returns (bytes memory)
    {
        // Authenticate before parsing: a slice on short data panics out of bounds and would
        // mask the NotVault revert.
        verifyCallback(data);
        return _handleCallback(data);
    }

    /// @dev Body of the callback, split from the authenticated entry point so a caller that is
    /// already the Vault can reach it without an external self-call, which would replace
    /// msg.sender and trip `vaultOnly`. Same split as UniswapV4Executor.
    function _handleCallback(bytes calldata data)
        internal
        returns (bytes memory)
    {
        // The payload is read at fixed offsets, so a callback that is not lockAcquired would be
        // misparsed into a pool key rather than rejected.
        bytes4 selector = bytes4(data[:4]);
        if (selector != _LOCK_ACQUIRED) {
            revert PancakeswapInfinityExecutor__UnexpectedCallback(selector);
        }
        // Both bounds: the ABI tail pads 149 to 160, so data[68:] carries 11 junk bytes.
        bytes calldata payload =
            data[_CALLBACK_HEADER:_CALLBACK_HEADER + _PAYLOAD_LENGTH];
        uint256 amountIn = uint256(bytes32(payload[0:32]));
        address receiver = address(bytes20(payload[32:52]));
        bytes calldata swapData = payload[52:_PAYLOAD_LENGTH];

        _swap(swapData, amountIn, receiver);
        // Dispatcher abi.decodes the result, so encode the empty.
        return abi.encode(bytes(""));
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 parameters,
            address hooks
        )
    {
        if (data.length != _DATA_LENGTH) {
            revert PancakeswapInfinityExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        zeroForOne = data[40] != 0;
        poolType = uint8(data[41]);
        if (poolType != _POOL_TYPE_CL && poolType != _POOL_TYPE_BIN) {
            revert PancakeswapInfinityExecutor__UnknownPoolType(poolType);
        }
        fee = uint24(bytes3(data[42:45]));
        parameters = bytes32(data[45:77]);
        hooks = address(bytes20(data[77:97]));
    }

    /// @dev Settles the input, swaps on whichever manager the pool-type byte selects and takes
    /// the output. Both amounts are read from the Vault, never from the delta a swap returns.
    function _swap(bytes calldata swapData, uint256 amountIn, address receiver)
        internal
    {
        (
            address tokenIn,
            address tokenOut,
            bool zeroForOne,
            uint8 poolType,
            uint24 fee,
            bytes32 parameters,
            address hooks
        ) = _decodeData(swapData);
        // Mapping first puts native ETH at currency0.
        (tokenIn, tokenOut) =
        (_toInfinityCurrency(tokenIn), _toInfinityCurrency(tokenOut));
        _settle(tokenIn, amountIn);
        // Swap what the Vault credited rather than what was sent, as UniswapV4Executor does: a
        // token that takes a fee on transfer credits less than amountIn.
        uint256 swapAmountIn = _getFullCredit(tokenIn);

        address manager =
            poolType == _POOL_TYPE_CL ? clPoolManager : binPoolManager;
        // Order from the direction byte, never by re-comparing addresses: the encoder owns
        // direction (CANONICAL note above).
        (address currency0, address currency1) =
            zeroForOne ? (tokenIn, tokenOut) : (tokenOut, tokenIn);
        // poolManager is part of the key and feeds the pool id hash, so a wrong one yields an
        // uninitialized pool and the swap reverts with PoolNotInitialized.
        InfinityPoolKey memory key = InfinityPoolKey({
            currency0: currency0,
            currency1: currency1,
            hooks: hooks,
            poolManager: manager,
            fee: fee,
            parameters: parameters
        });

        // Exact input is a NEGATIVE amountSpecified in both managers. Both deltas ignored on
        // purpose: take amount comes from _getFullCredit. Sizing a transfer from a returned
        // delta is forbidden (executor checklist in crates/tycho-execution/CLAUDE.md).
        if (poolType == _POOL_TYPE_CL) {
            // slither-disable-next-line unused-return
            IPancakeswapInfinityCLPoolManager(manager)
                .swap(
                    key,
                    IPancakeswapInfinityCLPoolManager.SwapParams({
                        zeroForOne: zeroForOne,
                        amountSpecified: -swapAmountIn.toInt256(),
                        sqrtPriceLimitX96: zeroForOne
                            ? TickMath.MIN_SQRT_PRICE + 1
                            : TickMath.MAX_SQRT_PRICE - 1
                    }),
                    ""
                );
        } else {
            // swapForY == zeroForOne: spend token0/X, receive token1/Y.
            // slither-disable-next-line unused-return
            IPancakeswapInfinityBinPoolManager(manager)
                .swap(key, zeroForOne, -swapAmountIn.toInt128(), "");
        }
        _take(tokenOut, receiver, _getFullCredit(tokenOut));
    }

    /// @dev Full amount the Vault owes this locker. address(this) is the router, not this
    /// contract: the Dispatcher delegatecalls, so the router is the recorded locker.
    function _getFullCredit(address currency)
        internal
        view
        returns (uint256 amount)
    {
        int256 _amount = vault.currencyDelta(address(this), currency);
        if (_amount < 0) {
            revert PancakeswapInfinityExecutor__DeltaNegative(currency);
        }
        amount = uint256(_amount);
    }

    /// @dev Pays the Vault. Native by value; ERC20 credited against the sync snapshot taken in
    /// swap, so the token must already have landed. No-op on 0.
    function _settle(address currency, uint256 amount) internal {
        if (amount == 0) return;
        if (currency == address(0)) {
            // slither-disable-next-line unused-return
            vault.settle{value: amount}();
        } else {
            // slither-disable-next-line unused-return
            vault.settle();
        }
    }

    /// @dev Infinity's native currency is address(0), the router's is ETH_ADDRESS.
    function _toInfinityCurrency(address token)
        internal
        pure
        returns (address)
    {
        return token == ETH_ADDRESS ? address(0) : token;
    }

    function _take(address currency, address recipient, uint256 amount)
        internal
    {
        if (amount == 0) return;
        vault.take(currency, recipient, amount);
    }

    /// @dev No pre-swap transfer: the input is settled into the Vault in the callback.
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
        (tokenIn, tokenOut,,,,,) = _decodeData(data);
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    /// @dev Dispatcher moves the input to the Vault just before handleCallback; native ETH the
    /// executor pays itself with settle{value}.
    function getCallbackTransferData(
        bytes calldata, /* data */
        address tokenIn,
        address /* caller */
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        if (tokenIn == ETH_ADDRESS) {
            return
                (
                    TransferManager.TransferType.TransferNativeInExecutor,
                    address(0)
                );
        }
        return (TransferManager.TransferType.Transfer, address(vault));
    }
}
