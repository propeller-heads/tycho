// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error PendleExecutor__InvalidDataLength();
error PendleExecutor__InvalidEdge();
error PendleExecutor__InvalidTarget();
error PendleExecutor__InvalidRouter();
error PendleExecutor__NativeEthUnsupported();

/// @notice The six Pendle edges: four through a market, two through its SY.
/// @dev The four market edges run through Pendle Router V4; the two SY edges call the SY
/// directly, since minting and redeeming SY is a constant-rate leg that needs no periphery.
enum PendleEdge {
    SyForPt,
    PtForSy,
    SyForYt,
    YtForSy,
    TokenForSy,
    SyForToken
}

contract PendleExecutor is IExecutor {
    /// @dev Pendle Router V4. Immutable rather than caller-supplied: the market in the
    /// calldata already lets the caller pick which market is traded, and there is no reason to
    /// also let it pick which contract is called.
    address public immutable router;

    constructor(address _router) {
        if (_router == address(0)) revert PendleExecutor__InvalidRouter();
        router = _router;
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

    /// @inheritdoc IExecutor
    /// @dev `minAmountOut` is zero on every call: slippage is enforced once, by the router, on
    /// the final output of the whole solution.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (PendleEdge edge, address tokenIn, address tokenOut, address target) =
            _decodeData(data);
        if (_isMarketEdge(edge)) {
            _checkMarketPair(edge, tokenIn, tokenOut, target);
        }

        if (edge == PendleEdge.SyForPt) {
            // An all-zero ApproxParams together with an empty LimitOrderData is what makes
            // Router V4 delegate to its on-chain approximation, which derives its own bounds
            // from live market state. Bounds computed at encoding time would be stale by the
            // time the executor runs against an amountIn that drifted.
            // slither-disable-next-line unused-return
            IPendleRouterV4(router)
                .swapExactSyForPt(
                    receiver,
                    target,
                    amountIn,
                    0,
                    ApproxParams(0, 0, 0, 0, 0),
                    _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.PtForSy) {
            // slither-disable-next-line unused-return
            IPendleRouterV4(router)
                .swapExactPtForSy(
                    receiver, target, amountIn, 0, _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.SyForYt) {
            // slither-disable-next-line unused-return
            IPendleRouterV4(router)
                .swapExactSyForYt(
                    receiver,
                    target,
                    amountIn,
                    0,
                    ApproxParams(0, 0, 0, 0, 0),
                    _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.YtForSy) {
            // slither-disable-next-line unused-return
            IPendleRouterV4(router)
                .swapExactYtForSy(
                    receiver, target, amountIn, 0, _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.TokenForSy) {
            if (tokenOut != target) revert PendleExecutor__InvalidTarget();
            // slither-disable-next-line unused-return
            IStandardizedYield(target).deposit(receiver, tokenIn, amountIn, 0);
        } else {
            if (tokenIn != target) revert PendleExecutor__InvalidTarget();
            // slither-disable-next-line unused-return
            IStandardizedYield(target)
                .redeem(receiver, amountIn, tokenOut, 0, false);
        }
    }

    /// @inheritdoc IExecutor
    /// @dev Router V4 and `SY.deposit` pull the input with `transferFrom`, so those edges approve
    /// the router or the SY. `SY.redeem` burns the caller's own shares instead, so a redemption
    /// names the caller as the receiver, which the Dispatcher never approves. Every edge takes an
    /// explicit receiver, so the output goes straight to it rather than back through the router.
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
        PendleEdge edge;
        address target;
        (edge, tokenIn, tokenOut, target) = _decodeData(data);

        if (_isMarketEdge(edge)) {
            receiver = router;
        } else if (edge == PendleEdge.SyForToken) {
            // msg.sender is the TychoRouter: getTransferData is staticcalled, so address(this)
            // would be this executor.
            receiver = msg.sender;
        } else {
            receiver = target;
        }
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;
    }

    /// @dev Router V4 trades whatever market it is handed, so this confirms the market trades
    /// the pair the calldata declares before any token moves, as the SY edges already check
    /// their target. Done in `swap` only: `getTransferData` is staticcalled on every hop, and the
    /// router's balance diff already catches an output token the swap did not produce.
    function _checkMarketPair(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        address market
    ) internal view {
        (address sy, address pt, address yt) = IPMarket(market).readTokens();
        address expectedIn;
        address expectedOut;
        if (edge == PendleEdge.SyForPt) {
            (expectedIn, expectedOut) = (sy, pt);
        } else if (edge == PendleEdge.PtForSy) {
            (expectedIn, expectedOut) = (pt, sy);
        } else if (edge == PendleEdge.SyForYt) {
            (expectedIn, expectedOut) = (sy, yt);
        } else {
            (expectedIn, expectedOut) = (yt, sy);
        }
        if (tokenIn != expectedIn || tokenOut != expectedOut) {
            revert PendleExecutor__InvalidTarget();
        }
    }

    function _isMarketEdge(PendleEdge edge) internal pure returns (bool) {
        return edge == PendleEdge.SyForPt || edge == PendleEdge.PtForSy
            || edge == PendleEdge.SyForYt || edge == PendleEdge.YtForSy;
    }

    /// @dev `target` is the market for the four market edges and the SY for the two SY edges.
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            PendleEdge edge,
            address tokenIn,
            address tokenOut,
            address target
        )
    {
        if (data.length != 61) {
            revert PendleExecutor__InvalidDataLength();
        }
        uint8 rawEdge = uint8(data[0]);
        if (rawEdge > uint8(type(PendleEdge).max)) {
            revert PendleExecutor__InvalidEdge();
        }
        edge = PendleEdge(rawEdge);
        tokenIn = address(bytes20(data[1:21]));
        tokenOut = address(bytes20(data[21:41]));
        target = address(bytes20(data[41:61]));
        // Every edge pulls an ERC-20 with transferFrom and forwards no value. The router names
        // native ETH by its sentinel and some SYs list it as the zero address.
        if (_isNative(tokenIn) || _isNative(tokenOut)) {
            revert PendleExecutor__NativeEthUnsupported();
        }
    }

    function _isNative(address token) internal pure returns (bool) {
        return token == ETH_ADDRESS || token == address(0);
    }

    function _emptyLimitOrderData()
        internal
        pure
        returns (LimitOrderData memory limit)
    {}
}

struct ApproxParams {
    uint256 guessMin;
    uint256 guessMax;
    uint256 guessOffchain;
    uint256 maxIteration;
    uint256 eps;
}

/// @dev Pendle's `LimitOrderData` as Router V4 declares it. Only the empty value is ever
/// passed — filling against the limit-order book is out of scope — but the struct still has to
/// be laid out exactly for the calldata to decode.
struct FillOrderParams {
    Order order;
    bytes signature;
    uint256 makingAmount;
}

struct Order {
    uint256 salt;
    uint256 expiry;
    uint256 nonce;
    uint8 orderType;
    address token;
    address YT;
    address maker;
    address receiver;
    uint256 makingAmount;
    uint256 lnImpliedRate;
    uint256 failSafeRate;
    bytes permit;
}

struct LimitOrderData {
    address limitRouter;
    uint256 epsSkipMarket;
    FillOrderParams[] normalFills;
    FillOrderParams[] flashFills;
    bytes optData;
}

interface IPendleRouterV4 {
    function swapExactSyForPt(
        address receiver,
        address market,
        uint256 exactSyIn,
        uint256 minPtOut,
        ApproxParams calldata guessPtOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netPtOut, uint256 netSyFee);

    function swapExactPtForSy(
        address receiver,
        address market,
        uint256 exactPtIn,
        uint256 minSyOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netSyOut, uint256 netSyFee);

    function swapExactSyForYt(
        address receiver,
        address market,
        uint256 exactSyIn,
        uint256 minYtOut,
        ApproxParams calldata guessYtOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netYtOut, uint256 netSyFee);

    function swapExactYtForSy(
        address receiver,
        address market,
        uint256 exactYtIn,
        uint256 minSyOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netSyOut, uint256 netSyFee);
}

interface IPMarket {
    function readTokens()
        external
        view
        returns (address sy, address pt, address yt);
}

interface IStandardizedYield is IERC20 {
    function deposit(
        address receiver,
        address tokenIn,
        uint256 amountTokenToDeposit,
        uint256 minSharesOut
    ) external payable returns (uint256 amountSharesOut);

    function redeem(
        address receiver,
        uint256 amountSharesToRedeem,
        address tokenOut,
        uint256 minTokenOut,
        bool burnFromInternalBalance
    ) external returns (uint256 amountTokenOut);
}
