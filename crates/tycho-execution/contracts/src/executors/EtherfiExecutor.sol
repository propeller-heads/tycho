// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error EtherfiExecutor__InvalidDataLength();
error EtherfiExecutor__InvalidDirection();
error EtherfiExecutor__ZeroAddress();
error EtherfiExecutor__NotAContract();

interface IEtherfiRedemptionManager {
    function redeemEEth(
        uint256 eEthAmount,
        address receiver,
        address outputToken
    ) external;
}

interface IEtherfiLiquidityPool {
    function deposit() external payable returns (uint256);
}

interface IWeETH {
    function wrap(uint256 _eETHAmount) external returns (uint256);

    function unwrap(uint256 _weETHAmount) external returns (uint256);
}

enum EtherfiDirection {
    EethToEth,
    EthToEeth,
    EethToWeeth,
    WeethToEeth
}

contract EtherfiExecutor is IExecutor {
    using SafeERC20 for IERC20;

    address public immutable ethAddress;
    address public immutable eethAddress;
    address public immutable liquidityPoolAddress;
    address public immutable weethAddress;
    address public immutable redemptionManagerAddress;

    constructor(
        address _ethAddress,
        address _eethAddress,
        address _liquidityPoolAddress,
        address _weethAddress,
        address _redemptionManagerAddress
    ) {
        if (_ethAddress == address(0)) {
            revert EtherfiExecutor__ZeroAddress();
        }
        if (
            _eethAddress == address(0) || _liquidityPoolAddress == address(0)
                || _weethAddress == address(0)
                || _redemptionManagerAddress == address(0)
        ) revert EtherfiExecutor__ZeroAddress();
        if (
            _eethAddress.code.length == 0
                || _liquidityPoolAddress.code.length == 0
                || _weethAddress.code.length == 0
                || _redemptionManagerAddress.code.length == 0
        ) revert EtherfiExecutor__NotAContract();

        ethAddress = _ethAddress;
        eethAddress = _eethAddress;
        liquidityPoolAddress = _liquidityPoolAddress;
        weethAddress = _weethAddress;
        redemptionManagerAddress = _redemptionManagerAddress;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        EtherfiDirection direction;
        direction = _decodeData(data);

        if (direction == EtherfiDirection.EethToEth) {
            IEtherfiRedemptionManager(redemptionManagerAddress)
                .redeemEEth(amountIn, receiver, ethAddress);
        } else if (direction == EtherfiDirection.EthToEeth) {
            // slither-disable-next-line arbitrary-send-eth,unused-return
            IEtherfiLiquidityPool(liquidityPoolAddress)
            .deposit{value: amountIn}();
        } else if (direction == EtherfiDirection.EethToWeeth) {
            // slither-disable-next-line unused-return
            IWeETH(weethAddress).wrap(amountIn);
        } else if (direction == EtherfiDirection.WeethToEeth) {
            // slither-disable-next-line unused-return
            IWeETH(weethAddress).unwrap(amountIn);
        } else {
            revert EtherfiExecutor__InvalidDirection();
        }
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
        EtherfiDirection direction = _decodeData(data);

        if (direction == EtherfiDirection.EthToEeth) {
            tokenIn = ETH_ADDRESS;
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            tokenOut = eethAddress;
            outputToRouter = true;
        } else if (direction == EtherfiDirection.EethToEth) {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = redemptionManagerAddress;
            tokenIn = eethAddress;
            tokenOut = ETH_ADDRESS;
            outputToRouter = false;
        } else if (direction == EtherfiDirection.EethToWeeth) {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = weethAddress;
            tokenIn = eethAddress;
            tokenOut = weethAddress;
            outputToRouter = true;
        } else if (direction == EtherfiDirection.WeethToEeth) {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = msg.sender;
            tokenIn = weethAddress;
            tokenOut = eethAddress;
            outputToRouter = true;
        } else {
            revert EtherfiExecutor__InvalidDirection();
        }
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address)
    {
        return msg.sender;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (EtherfiDirection direction)
    {
        if (data.length != 1) {
            revert EtherfiExecutor__InvalidDataLength();
        }
        direction = EtherfiDirection(uint8(data[0]));
    }
}
