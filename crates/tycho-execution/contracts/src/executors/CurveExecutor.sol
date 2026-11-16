// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransferManager} from "../TransferManager.sol";
import {
    ICurveCryptoPool,
    ICurveCryptoPoolETH,
    ICurveStablePool,
    isCurveStablePool
} from "@interfaces/ICurvePool.sol";

error CurveExecutor__AddressZero();
error CurveExecutor__InvalidDataLength();
error CurveExecutor__TokenAddressZero();
error CurveExecutor__NotAContract(address addr);

contract CurveExecutor is IExecutor {
    using SafeERC20 for IERC20;

    address public immutable nativeToken;
    address public immutable stEthAddress;
    bool public immutable hasStETH;

    // forge-lint: disable-next-line(missing-zero-check) address(0) means the chain has no stETH pool.
    constructor(address nativeToken_, address stEthAddress_) {
        if (nativeToken_ == address(0)) {
            revert CurveExecutor__AddressZero();
        }
        nativeToken = nativeToken_;

        if (stEthAddress_ != address(0)) {
            hasStETH = true;
        } else {
            hasStETH = false;
        }
        stEthAddress = stEthAddress_;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        if (data.length != 63) {
            revert CurveExecutor__InvalidDataLength();
        }
        address tokenIn;
        address tokenOut;
        address pool;
        uint8 poolType;
        uint8 i;
        uint8 j;
        (tokenIn, tokenOut, pool, poolType, i, j) = _decodeData(data);

        uint256 ethAmount = 0;
        if (tokenIn == nativeToken) {
            ethAmount = amountIn;
        }

        if (isCurveStablePool(poolType)) {
            // forge-lint: disable-next-item(arbitrary-send-eth) Sends the swap input to the pool the swap data names.
            ICurveStablePool(pool).exchange{value: ethAmount}(
                int128(uint128(i)), int128(uint128(j)), amountIn, 0
            );
        } else {
            // crypto or llamma
            if (tokenIn == nativeToken || tokenOut == nativeToken) {
                // forge-lint: disable-next-item(arbitrary-send-eth) Sends the swap input to the pool the swap data names.
                ICurveCryptoPoolETH(pool).exchange{value: ethAmount}(
                    i, j, amountIn, 0, true
                );
            } else {
                ICurveCryptoPool(pool).exchange(i, j, amountIn, 0);
            }
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            address pool,
            uint8 poolType,
            uint8 i,
            uint8 j
        )
    {
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        if (tokenIn == address(0) || tokenOut == address(0)) {
            revert CurveExecutor__TokenAddressZero();
        }
        pool = address(bytes20(data[40:60]));
        poolType = uint8(data[60]);
        i = uint8(data[61]);
        j = uint8(data[62]);
    }

    /**
     * @dev Even though this contract is mostly called through delegatecall, we still want to be able to receive ETH.
     * This is needed when using the executor directly and it makes testing easier.
     * There are some curve pools that take ETH directly.
     */
    receive() external payable {
        require(
            msg.sender.code.length != 0, CurveExecutor__NotAContract(msg.sender)
        );
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        if (tokenIn == address(0) || tokenOut == address(0)) {
            revert CurveExecutor__TokenAddressZero();
        }
        if (tokenIn == nativeToken) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
        } else {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
        }
        // The receiver of the funds will be the pool contract. This is only relevant
        // for performing an approval in the case of ProtocolWillDebit.
        receiver = address(bytes20(data[40:60]));
        outputToRouter = true;
    }
}
