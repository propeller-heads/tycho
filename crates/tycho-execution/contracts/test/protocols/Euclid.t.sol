pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/EuclidExecutor.sol";
import {Constants} from "../Constants.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev AggregationRouterV5.fillOrderRFQTo
bytes4 constant FILL_ORDER_RFQ_TO_SELECTOR = 0x5a099843;

/// @dev 1inch Aggregation Router v5 — Euclid's settlement target
address constant ONEINCH_ROUTER_V5 = 0x1111111254EEB25477B68fb85Ed929f73A960582;

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
            address target,
            uint8 partialFillOffset,
            uint256 originalFilledTakerAmount,
            bytes memory euclidCalldata
        )
    {
        return _decodeData(data);
    }
}

contract EuclidExecutorTest is Constants, TestUtils {
    EuclidExecutorExposed euclidExecutor;

    /// @dev Euclid maker wallet stand-in — orders are signed with this key.
    uint256 constant MAKER_PK = 0xE0C11D;
    /// @dev fillOrderRFQTo: 7 inline order words + 1 signature-offset word.
    uint8 constant PARTIAL_FILL_OFFSET = 8;

    /// @dev EIP-712 typehash for 1inch LOP v3 OrderRFQ
    bytes32 constant ORDER_RFQ_TYPEHASH = keccak256(
        "OrderRFQ(uint256 info,address makerAsset,address takerAsset,address maker,address allowedSender,uint256 makingAmount,uint256 takingAmount)"
    );

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
    ///      API does: signed order at the quoted amounts, plain taker amount
    ///      in the flagsAndAmount word, output to `receiver`.
    function _buildEuclidCalldata(
        address makerAsset,
        address takerAsset,
        uint256 makingAmount,
        uint256 takingAmount,
        uint256 filledTakerAmount,
        address receiver
    ) internal view returns (bytes memory) {
        address maker = vm.addr(MAKER_PK);
        IAggregationRouterV5.OrderRFQ memory order =
            IAggregationRouterV5.OrderRFQ({
                info: (uint256(block.timestamp + 300) << 64) | 1, // expiry | nonce
                makerAsset: makerAsset,
                takerAsset: takerAsset,
                maker: maker,
                allowedSender: address(0),
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

    function _fundMaker(address makerAsset, uint256 amount) internal {
        address maker = vm.addr(MAKER_PK);
        deal(makerAsset, maker, amount);
        vm.prank(maker);
        IERC20(makerAsset).approve(ONEINCH_ROUTER_V5, amount);
    }

    function testDecodeData() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory euclidCalldata = abi.encodePacked(
            FILL_ORDER_RFQ_TO_SELECTOR,
            hex"0000000000000000000000000000000000000000000000000000000000000001"
        );

        uint256 originalAmountIn = 1 ether;
        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            originalAmountIn,
            euclidCalldata
        );

        (
            address decodedTarget,
            uint8 decodedPartialFillOffset,
            uint256 decodedOriginalAmountIn,
            bytes memory decodedEuclidCalldata
        ) = euclidExecutor.decodeData(params);

        assertEq(decodedTarget, ONEINCH_ROUTER_V5, "target mismatch");
        assertEq(
            keccak256(decodedEuclidCalldata),
            keccak256(euclidCalldata),
            "euclidCalldata mismatch"
        );
        assertEq(
            decodedPartialFillOffset,
            PARTIAL_FILL_OFFSET,
            "partialFillOffset mismatch"
        );
        assertEq(
            decodedOriginalAmountIn,
            originalAmountIn,
            "originalAmountIn mismatch"
        );
    }

    function testGetTransferData() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory euclidCalldata =
            abi.encodePacked(FILL_ORDER_RFQ_TO_SELECTOR, uint256(1));
        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            uint256(1 ether),
            euclidCalldata
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = euclidExecutor.getTransferData(params);

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

    function testGetTransferData_RevertsOnUnknownTarget() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            BOB, // not the settlement router
            PARTIAL_FILL_OFFSET,
            uint256(1 ether),
            abi.encodePacked(FILL_ORDER_RFQ_TO_SELECTOR, uint256(1))
        );

        vm.expectRevert(EuclidExecutor.EuclidExecutor__InvalidTarget.selector);
        euclidExecutor.getTransferData(params);
    }

    function testSwap_FullFill() public {
        // 1 WETH -> USDC at a quoted rate of 3500, signed by the Euclid maker
        // and filled through the real 1inch v5 router on a mainnet fork.
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000; // 3500 USDC

        _fundMaker(USDC_ADDR, makingAmount);
        deal(WETH_ADDR, address(euclidExecutor), takingAmount);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, takingAmount);

        bytes memory euclidCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount,
            address(euclidExecutor)
        );

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            takingAmount,
            euclidCalldata
        );

        euclidExecutor.swap(takingAmount, params, address(euclidExecutor));

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
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;
        uint256 actualAmountIn = takingAmount / 2;

        _fundMaker(USDC_ADDR, makingAmount);
        deal(WETH_ADDR, address(euclidExecutor), actualAmountIn);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, actualAmountIn);

        bytes memory euclidCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount, // quoted fill amount — executor patches this down
            address(euclidExecutor)
        );

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            takingAmount, // original quoted amount
            euclidCalldata
        );

        euclidExecutor.swap(actualAmountIn, params, address(euclidExecutor));

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

    function testSwap_RevertsOnInvalidTarget() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            BOB, // not the settlement router
            PARTIAL_FILL_OFFSET,
            uint256(1 ether),
            abi.encodePacked(FILL_ORDER_RFQ_TO_SELECTOR, uint256(1))
        );

        vm.expectRevert(EuclidExecutor.EuclidExecutor__InvalidTarget.selector);
        euclidExecutor.swap(1 ether, params, address(euclidExecutor));
    }

    function testSwap_RevertsOnInvalidSelector() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            uint256(1 ether),
            abi.encodePacked(bytes4(0xdeadbeef), uint256(1))
        );

        vm.expectRevert(EuclidExecutor.EuclidExecutor__InvalidSelector.selector);
        euclidExecutor.swap(1 ether, params, address(euclidExecutor));
    }

    function testConstructor_RevertsOnZeroAddress() public {
        vm.expectRevert(EuclidExecutor.EuclidExecutor__ZeroAddress.selector);
        new EuclidExecutorExposed(address(0));
    }

    function testDecodeData_RevertsOnShortData() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        bytes memory tooShort = new bytes(92); // one byte under the fixed fields
        vm.expectRevert(
            EuclidExecutor.EuclidExecutor__InvalidDataLength.selector
        );
        euclidExecutor.decodeData(tooShort);

        vm.expectRevert(
            EuclidExecutor.EuclidExecutor__InvalidDataLength.selector
        );
        euclidExecutor.getTransferData(tooShort);
    }

    function testSwap_ExcessAmountInNeverPatchesUp() public {
        // Router hands the executor MORE than the quote was signed for; the
        // fill amount must stay at the quoted value — never patched upward.
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;
        uint256 excessAmountIn = takingAmount * 2;

        _fundMaker(USDC_ADDR, makingAmount);
        deal(WETH_ADDR, address(euclidExecutor), excessAmountIn);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, excessAmountIn);

        bytes memory euclidCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount,
            address(euclidExecutor)
        );

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            takingAmount, // original quoted amount
            euclidCalldata
        );

        euclidExecutor.swap(excessAmountIn, params, address(euclidExecutor));

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

    function testSwap_RevertsOnExpiredOrder() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;

        _fundMaker(USDC_ADDR, makingAmount);
        deal(WETH_ADDR, address(euclidExecutor), takingAmount);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, takingAmount);

        // Warp past the order expiry after signing.
        bytes memory euclidCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount,
            address(euclidExecutor)
        );
        vm.warp(block.timestamp + 301);

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            takingAmount,
            euclidCalldata
        );

        vm.expectRevert(); // 1inch: order expired
        euclidExecutor.swap(takingAmount, params, address(euclidExecutor));
    }

    function testSwap_RevertsOnBadSignature() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);

        uint256 takingAmount = 1 ether;
        uint256 makingAmount = 3500_000000;

        _fundMaker(USDC_ADDR, makingAmount);
        deal(WETH_ADDR, address(euclidExecutor), takingAmount);
        vm.prank(address(euclidExecutor));
        IERC20(WETH_ADDR).approve(ONEINCH_ROUTER_V5, takingAmount);

        bytes memory euclidCalldata = _buildEuclidCalldata(
            USDC_ADDR,
            WETH_ADDR,
            makingAmount,
            takingAmount,
            takingAmount,
            address(euclidExecutor)
        );
        // Corrupt one signature byte (the signature is the tail-adjacent
        // dynamic arg; flip a byte deep in the calldata body).
        euclidCalldata[euclidCalldata.length - 40] ^= 0xff;

        bytes memory params = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            ONEINCH_ROUTER_V5,
            PARTIAL_FILL_OFFSET,
            takingAmount,
            euclidCalldata
        );

        vm.expectRevert(); // 1inch: bad signature
        euclidExecutor.swap(takingAmount, params, address(euclidExecutor));
    }

    function testFundsExpectedAddress() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);
        euclidExecutor = new EuclidExecutorExposed(ONEINCH_ROUTER_V5);
        assertEq(
            euclidExecutor.fundsExpectedAddress(hex""),
            address(this),
            "funds expected at caller"
        );
    }
}
