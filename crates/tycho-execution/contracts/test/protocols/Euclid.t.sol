pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/EuclidExecutor.sol";
import {Constants} from "../Constants.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev AggregationRouterV5.fillOrderRFQTo
bytes4 constant FILL_ORDER_RFQ_TO_SELECTOR = 0x5a099843;

/// @dev The 1inch v5 router's own reverts, from OrderRFQMixin.
error OrderExpired();
error RFQBadSignature();
error RFQPrivateOrder();

interface IAggregationRouterV5 {
    struct OrderRFQ {
        uint256 info;
        address makerAsset;
        address takerAsset;
        address maker;
        address allowedSender;
        uint256 makingAmount;
        uint256 takingAmount;
    }

    function fillOrderRFQTo(
        OrderRFQ calldata order,
        bytes calldata signature,
        uint256 flagsAndAmount,
        address target
    ) external payable returns (uint256, uint256, bytes32);
}

contract EuclidExecutorExposed is EuclidExecutor {
    constructor(address _euclidSettlement) EuclidExecutor(_euclidSettlement) {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            address tokenOut,
            uint256 originalFilledTakerAmount,
            bytes memory fillCalldata
        )
    {
        return _decodeData(data);
    }
}

contract EuclidExecutorTest is Constants, TestUtils {
    EuclidExecutorExposed euclidExecutor;

    /// @dev Euclid maker wallet stand-in — orders are signed with this key.
    uint256 constant MAKER_PK = 0xE0C11D;

    /// @dev EIP-712 typehash for 1inch LOP v3 OrderRFQ
    bytes32 constant ORDER_RFQ_TYPEHASH = keccak256(
        "OrderRFQ(uint256 info,address makerAsset,address takerAsset,address maker,address allowedSender,uint256 makingAmount,uint256 takingAmount)"
    );

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);
    }

    function _domainSeparator() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256("1inch Aggregation Router"),
                keccak256("5"),
                block.chainid,
                ONEINCH_ROUTER_V5
            )
        );
    }

    /// @dev Builds a maker-signed fillOrderRFQTo call the way the Euclid RFQ
    ///      gateway does: signed order at the quoted amounts, admitting only
    ///      `allowedSender`, plain taker amount in the flagsAndAmount word,
    ///      output to `receiver`.
    function _buildEuclidCalldata(
        address makerAsset,
        address takerAsset,
        uint256 makingAmount,
        uint256 takingAmount,
        uint256 filledTakerAmount,
        address allowedSender,
        address receiver
    ) internal view returns (bytes memory) {
        address maker = vm.addr(MAKER_PK);
        IAggregationRouterV5.OrderRFQ memory order =
            IAggregationRouterV5.OrderRFQ({
                info: (uint256(block.timestamp + 300) << 64) | 1, // expiry | nonce
                makerAsset: makerAsset,
                takerAsset: takerAsset,
                maker: maker,
                allowedSender: allowedSender,
                makingAmount: makingAmount,
                takingAmount: takingAmount
            });

        bytes32 structHash = keccak256(
            abi.encode(
                ORDER_RFQ_TYPEHASH,
                order.info,
                order.makerAsset,
                order.takerAsset,
                order.maker,
                order.allowedSender,
                order.makingAmount,
                order.takingAmount
            )
        );
        bytes32 digest = keccak256(
            abi.encodePacked("\x19\x01", _domainSeparator(), structHash)
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(MAKER_PK, digest);
        bytes memory signature = abi.encodePacked(r, s, v);

        return abi.encodeCall(
            IAggregationRouterV5.fillOrderRFQTo,
            (order, signature, filledTakerAmount, receiver)
        );
    }

    /// @dev A WETH -> USDC order for the executor itself to fill, as the unit
    ///      tests call it directly: it is the taker 1inch sees.
    function _wethUsdcCalldata(
        uint256 makingAmount,
        uint256 takingAmount,
        uint256 filledTakerAmount
    ) internal view returns (bytes memory) {
        return _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            filledTakerAmount,
            address(euclidExecutor),
            address(euclidExecutor)
        );
    }

    /// @dev The executor's swap data: tokens, the signed amount, the calldata.
    function _params(
        uint256 originalFilledTakerAmount,
        bytes memory fillCalldata
    ) internal view returns (bytes memory) {
        return abi.encodePacked(
            WETH_ADDR, USDC_ADDR, originalFilledTakerAmount, fillCalldata
        );
    }

    function _fundMaker(address makerAsset, uint256 amount) internal {
        address maker = vm.addr(MAKER_PK);
        deal(makerAsset, maker, amount);
        vm.prank(maker);
        IERC20(makerAsset).approve(ONEINCH_ROUTER_V5, amount);
    }

    function _fundExecutor(uint256 wethAmount) internal {
        deal(WETH_ADDR, address(euclidExecutor), wethAmount);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, wethAmount);
    }

    /// @dev A fill call of the right length whose selector is `selector`.
    function _fillCalldataWithSelector(bytes4 selector)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(selector, new bytes(10 * 32));
    }

    function testDecodeData() public view {
        bytes memory fillCalldata =
            _fillCalldataWithSelector(FILL_ORDER_RFQ_TO_SELECTOR);
        uint256 originalAmountIn = 1 ether;

        (
            address tokenIn,
            address tokenOut,
            uint256 decodedOriginalAmountIn,
            bytes memory decodedFillCalldata
        ) = euclidExecutor.decodeData(_params(originalAmountIn, fillCalldata));

        assertEq(tokenIn, WETH_ADDR, "tokenIn mismatch");
        assertEq(tokenOut, USDC_ADDR, "tokenOut mismatch");
        assertEq(
            decodedOriginalAmountIn,
            originalAmountIn,
            "originalAmountIn mismatch"
        );
        assertEq(
            keccak256(decodedFillCalldata),
            keccak256(fillCalldata),
            "fillCalldata mismatch"
        );
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = euclidExecutor.getTransferData(
            _params(
                1 ether, _fillCalldataWithSelector(FILL_ORDER_RFQ_TO_SELECTOR)
            )
        );

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit),
            "transferType mismatch"
        );
        assertEq(receiver, ONEINCH_ROUTER_V5, "receiver mismatch");
        assertEq(tokenIn, WETH_ADDR, "tokenIn mismatch");
        assertEq(tokenOut, USDC_ADDR, "tokenOut mismatch");
        assertEq(outputToRouter, true, "outputToRouter mismatch");
    }

    function testSwap_FullFill() public {
        // 1 WETH -> USDC at a quoted rate of 3500, signed by the Euclid maker
        // and filled through the real 1inch v5 router on a mainnet fork.
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000; // 3500 USDC

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(takingAmount);

        euclidExecutor.swap(
            takingAmount,
            _params(
                takingAmount,
                _wethUsdcCalldata(makingAmount, takingAmount, takingAmount)
            ),
            address(euclidExecutor)
        );

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(euclidExecutor)),
            makingAmount,
            "usdc should be at receiver"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(euclidExecutor)),
            0,
            "weth left in executor"
        );
    }

    function testSwap_PartialFillPatchesDown() public {
        // Quote signed for 1 WETH, but an earlier route leg only delivered
        // 0.5 WETH — the executor patches the fill amount down and the maker
        // pays out proportionally.
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;
        uint256 actualAmountIn = takingAmount / 2;

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(actualAmountIn);

        euclidExecutor.swap(
            actualAmountIn,
            _params(
                takingAmount, // original quoted amount
                _wethUsdcCalldata(makingAmount, takingAmount, takingAmount)
            ),
            address(euclidExecutor)
        );

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(euclidExecutor)),
            makingAmount / 2,
            "usdc should be half the quote"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(euclidExecutor)),
            0,
            "weth left in executor"
        );
    }

    function testSwap_ExcessAmountInNeverPatchesUp() public {
        // Router hands the executor MORE than the quote was signed for; the
        // fill amount must stay at the quoted value — never patched upward.
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;
        uint256 excessAmountIn = takingAmount * 2;

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(excessAmountIn);

        euclidExecutor.swap(
            excessAmountIn,
            _params(
                takingAmount, // original quoted amount
                _wethUsdcCalldata(makingAmount, takingAmount, takingAmount)
            ),
            address(euclidExecutor)
        );

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(euclidExecutor)),
            makingAmount,
            "maker must pay exactly the quoted amount"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(euclidExecutor)),
            excessAmountIn - takingAmount,
            "only the quoted taker amount may be pulled"
        );
    }

    function testSwap_RevertsOnInvalidSelector() public {
        vm.expectRevert(EuclidExecutor.EuclidExecutor__InvalidSelector.selector);
        euclidExecutor.swap(
            1 ether,
            _params(1 ether, _fillCalldataWithSelector(bytes4(0xdeadbeef))),
            address(euclidExecutor)
        );
    }

    function testConstructor_RevertsOnZeroAddress() public {
        vm.expectRevert(EuclidExecutor.EuclidExecutor__ZeroAddress.selector);
        new EuclidExecutorExposed(address(0));
    }

    /// @dev The data must carry the header and a whole fill call: one byte
    ///      under that, and the amount word the executor patches would lie
    ///      outside the calldata.
    function testDecodeData_RevertsOnShortData() public {
        bytes memory tooShort = new bytes(72 + 4 + 10 * 32 - 1);

        vm.expectRevert(
            EuclidExecutor.EuclidExecutor__InvalidDataLength.selector
        );
        euclidExecutor.decodeData(tooShort);

        vm.expectRevert(
            EuclidExecutor.EuclidExecutor__InvalidDataLength.selector
        );
        euclidExecutor.getTransferData(tooShort);

        vm.expectRevert(
            EuclidExecutor.EuclidExecutor__InvalidDataLength.selector
        );
        euclidExecutor.swap(1 ether, tooShort, address(euclidExecutor));
    }

    function testSwap_RevertsOnExpiredOrder() public {
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(takingAmount);

        // Warp past the order expiry after signing.
        bytes memory fillCalldata =
            _wethUsdcCalldata(makingAmount, takingAmount, takingAmount);
        vm.warp(block.timestamp + 301);

        vm.expectRevert(OrderExpired.selector);
        euclidExecutor.swap(
            takingAmount,
            _params(takingAmount, fillCalldata),
            address(euclidExecutor)
        );
    }

    function testSwap_RevertsOnBadSignature() public {
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(takingAmount);

        bytes memory fillCalldata =
            _wethUsdcCalldata(makingAmount, takingAmount, takingAmount);
        // Corrupt one signature byte (the signature is the tail-adjacent
        // dynamic arg; flip a byte deep in the calldata body).
        fillCalldata[fillCalldata.length - 40] ^= 0xff;

        vm.expectRevert(RFQBadSignature.selector);
        euclidExecutor.swap(
            takingAmount,
            _params(takingAmount, fillCalldata),
            address(euclidExecutor)
        );
    }

    /// @dev The gateway signs each order for the sender that asked, so a
    ///      filler the order does not admit is refused by 1inch.
    function testSwap_RevertsForAnotherFiller() public {
        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;

        _fundMaker(USDC_ADDR, makingAmount);
        _fundExecutor(takingAmount);

        bytes memory fillCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount,
            BOB, // admitted filler
            address(euclidExecutor)
        );

        vm.expectRevert(RFQPrivateOrder.selector);
        euclidExecutor.swap(
            takingAmount,
            _params(takingAmount, fillCalldata),
            address(euclidExecutor)
        );
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            euclidExecutor.fundsExpectedAddress(hex""),
            address(this),
            "funds expected at caller"
        );
    }
}

/// @dev Runs calldata the Rust encoder produced for a quote recorded from
///      Euclid's gateway through the router. The order was requested for the
///      router this setup deploys, and signed for it, so the fork is pinned to
///      the block the quote was recorded at, where the maker's inventory and
///      allowance stood behind it.
contract TychoRouterForEuclidTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 26384407;
    }

    /// @dev The user the Rust test encodes for; WETH in, USDC out.
    address constant USER = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
    uint256 constant QUOTED_AMOUNT_IN = 3_000_000_000_000_000; // 0.003 WETH
    uint256 constant QUOTED_AMOUNT_OUT = 6241692;

    function _swapThroughRouter(uint256 amountIn, string memory calldataName)
        internal
        returns (uint256 usdcReceived)
    {
        deal(WETH_ADDR, USER, amountIn);
        uint256 usdcBefore = IERC20(USDC_ADDR).balanceOf(USER);

        vm.startPrank(USER);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) =
            tychoRouterAddr.call(loadCallDataFromFile(calldataName));
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        usdcReceived = IERC20(USDC_ADDR).balanceOf(USER) - usdcBefore;
        assertEq(
            IERC20(WETH_ADDR).balanceOf(tychoRouterAddr),
            0,
            "weth left in router"
        );
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "usdc left in router"
        );
    }

    function testSingleEuclidIntegration() public {
        uint256 usdcReceived = _swapThroughRouter(
            QUOTED_AMOUNT_IN, "test_single_encoding_strategy_euclid"
        );
        assertEq(usdcReceived, QUOTED_AMOUNT_OUT, "usdc received");
    }

    /// @dev Half the quoted amount reaches the executor: it lowers the fill
    ///      word and 1inch pays the maker's amount pro rata, rounded down.
    function testEuclidPartialFillThroughRouter() public {
        uint256 usdcReceived = _swapThroughRouter(
            QUOTED_AMOUNT_IN / 2,
            "test_single_encoding_strategy_euclid_partial_fill"
        );
        assertEq(usdcReceived, QUOTED_AMOUNT_OUT / 2, "usdc received");
    }
}
