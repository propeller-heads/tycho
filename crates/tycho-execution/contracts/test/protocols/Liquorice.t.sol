// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/LiquoriceExecutor.sol";
import {Constants} from "../Constants.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @dev Mirrors the deployed settlement at LIQUORICE_SETTLEMENT. The tuple shapes are what the
///      selectors are derived from, so they must match the deployed ABI field for field.
interface ILiquoriceSettlement {
    struct BaseTokenData {
        address addr;
        uint256 amount;
        uint256 toRecipient;
        uint256 toRepay;
        uint256 toSupply;
        bool lockedCollateral;
    }

    struct QuoteTokenData {
        address addr;
        uint256 amount;
        uint256 toTrader;
        uint256 toWithdraw;
        uint256 toBorrow;
        bool lockedCollateral;
    }

    struct Order {
        string rfqId;
        uint256 nonce;
        address trader;
        address effectiveTrader;
        uint256 quoteExpiry;
        address recipient;
        uint256 minFillAmount;
        uint8 makerFlags;
        BaseTokenData baseTokenData;
        QuoteTokenData quoteTokenData;
    }

    struct Single {
        string rfqId;
        uint256 nonce;
        address trader;
        address effectiveTrader;
        address baseToken;
        address quoteToken;
        uint256 baseTokenAmount;
        uint256 quoteTokenAmount;
        uint256 minFillAmount;
        uint256 quoteExpiry;
        address recipient;
    }

    struct Interaction {
        address target;
        uint256 value;
        bytes callData;
    }

    struct Hooks {
        Interaction[] beforeSettle;
        Interaction[] afterSettle;
    }

    struct TypedSignature {
        uint8 signatureType;
        uint8 transferCommand;
        bytes signatureBytes;
    }

    function BALANCE_MANAGER() external view returns (address);

    function AUTHENTICATOR() external view returns (address);

    function hashOrder(Order calldata order) external view returns (bytes32);

    function hashSingleOrder(Single calldata order)
        external
        view
        returns (bytes32);

    function settle(
        address signer,
        uint256 filledTakerAmount,
        Order calldata order,
        Interaction[] calldata interactions,
        Hooks calldata hooks,
        TypedSignature calldata makerSignature,
        TypedSignature calldata takerSignature
    ) external;

    function settleSingle(
        address signer,
        Single calldata order,
        TypedSignature calldata makerSignature,
        uint256 filledTakerAmount,
        TypedSignature calldata takerSignature
    ) external payable;
}

interface IAllowListAuthentication {
    function addSolver(address _solver) external;

    function addMaker(address _maker) external;

    function isMaker(address _addr) external view returns (bool);

    function isSolver(address _addr) external view returns (bool);
}

contract LiquoriceExecutorExposed is LiquoriceExecutor {
    constructor(address _liquoriceSettlement, address _liquoriceBalanceManager)
        LiquoriceExecutor(_liquoriceSettlement, _liquoriceBalanceManager)
    {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            uint32 partialFillOffset,
            uint256 originalBaseTokenAmount,
            uint256 minBaseTokenAmount,
            bytes memory liquoriceCalldata
        )
    {
        return _decodeData(data);
    }

    function clampAmount(
        uint256 givenAmount,
        uint256 originalBaseTokenAmount,
        uint256 minBaseTokenAmount
    ) external pure returns (uint256) {
        return _clampAmount(
            givenAmount, originalBaseTokenAmount, minBaseTokenAmount
        );
    }
}

contract LiquoriceExecutorTest is Constants, Permit2TestHelper, TestUtils {
    using SafeERC20 for IERC20;

    ILiquoriceSettlement liquoriceSettlement;
    IAllowListAuthentication authenticator;
    LiquoriceExecutorExposed liquoriceExecutor;

    uint256 constant MAKER_PK = 0xA11CE;
    address maker;

    // Signature.Type.EIP712 and Signature.TransferCommand.SIMPLE_TRANSFER on the settlement.
    uint8 constant SIG_EIP712 = 3;
    uint8 constant TRANSFER_SIMPLE = 1;
    // Signature.MakerFlags.NONE: no lending-pool leg, so the order settles as a plain swap.
    uint8 constant MAKER_FLAGS_NONE = 0;

    uint256 constant FORK_BLOCK = 25_900_000;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        liquoriceSettlement = ILiquoriceSettlement(LIQUORICE_SETTLEMENT);
        maker = vm.addr(MAKER_PK);

        liquoriceExecutor = new LiquoriceExecutorExposed(
            LIQUORICE_SETTLEMENT, LIQUORICE_BALANCE_MANAGER
        );
        authenticator =
            IAllowListAuthentication(liquoriceSettlement.AUTHENTICATOR());

        // The settlement gates callers and makers behind its own allowlist, which is governed by
        // Liquorice. Neither this executor nor the test maker is on it.
        vm.mockCall(
            address(authenticator),
            abi.encodeWithSelector(
                IAllowListAuthentication.isSolver.selector,
                address(liquoriceExecutor)
            ),
            abi.encode(true)
        );
        vm.mockCall(
            address(authenticator),
            abi.encodeWithSelector(
                IAllowListAuthentication.isMaker.selector, maker
            ),
            abi.encode(true)
        );

        vm.prank(maker);
        IERC20(WETH_ADDR).approve(LIQUORICE_BALANCE_MANAGER, type(uint256).max);
    }

    /// @dev The balance manager the router approves must be the one this settlement pulls through.
    function testBalanceManagerBelongsToSettlement() public view {
        assertEq(
            liquoriceSettlement.BALANCE_MANAGER(),
            LIQUORICE_BALANCE_MANAGER,
            "balance manager is not this settlement's"
        );
    }

    function _singleOrder(uint256 amountIn, uint256 amountOut)
        internal
        view
        returns (ILiquoriceSettlement.Single memory order)
    {
        order = ILiquoriceSettlement.Single({
            rfqId: "tycho-test",
            nonce: uint256(keccak256("tycho-test-single")),
            trader: address(liquoriceExecutor),
            effectiveTrader: address(liquoriceExecutor),
            baseToken: USDC_ADDR,
            quoteToken: WETH_ADDR,
            baseTokenAmount: amountIn,
            quoteTokenAmount: amountOut,
            minFillAmount: 0,
            quoteExpiry: block.timestamp + 1 hours,
            recipient: maker
        });
    }

    function _order(uint256 amountIn, uint256 amountOut)
        internal
        view
        returns (ILiquoriceSettlement.Order memory order)
    {
        order = ILiquoriceSettlement.Order({
            rfqId: "tycho-test",
            nonce: uint256(keccak256("tycho-test-order")),
            trader: address(liquoriceExecutor),
            effectiveTrader: address(liquoriceExecutor),
            quoteExpiry: block.timestamp + 1 hours,
            recipient: maker,
            minFillAmount: 0,
            makerFlags: MAKER_FLAGS_NONE,
            baseTokenData: ILiquoriceSettlement.BaseTokenData({
                addr: USDC_ADDR,
                amount: amountIn,
                toRecipient: amountIn,
                toRepay: 0,
                toSupply: 0,
                lockedCollateral: false
            }),
            quoteTokenData: ILiquoriceSettlement.QuoteTokenData({
                addr: WETH_ADDR,
                amount: amountOut,
                toTrader: amountOut,
                toWithdraw: 0,
                toBorrow: 0,
                lockedCollateral: false
            })
        });
    }

    function _sign(bytes32 digest)
        internal
        pure
        returns (ILiquoriceSettlement.TypedSignature memory)
    {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(MAKER_PK, digest);
        return ILiquoriceSettlement.TypedSignature({
            signatureType: SIG_EIP712,
            transferCommand: TRANSFER_SIMPLE,
            signatureBytes: abi.encodePacked(r, s, v)
        });
    }

    /// @dev The taker signature is empty: the settlement then requires the caller to be the
    ///      effective trader, which the executor is here and the router is in production.
    function _emptyTakerSignature()
        internal
        pure
        returns (ILiquoriceSettlement.TypedSignature memory)
    {
        return ILiquoriceSettlement.TypedSignature({
            signatureType: SIG_EIP712,
            transferCommand: TRANSFER_SIMPLE,
            signatureBytes: ""
        });
    }

    function _encodeSettleSingle(uint256 amountIn, uint256 amountOut)
        internal
        view
        returns (bytes memory)
    {
        ILiquoriceSettlement.Single memory order =
            _singleOrder(amountIn, amountOut);
        return abi.encodeCall(
            ILiquoriceSettlement.settleSingle,
            (
                maker,
                order,
                _sign(liquoriceSettlement.hashSingleOrder(order)),
                amountIn,
                _emptyTakerSignature()
            )
        );
    }

    function _encodeSettle(uint256 amountIn, uint256 amountOut)
        internal
        view
        returns (bytes memory)
    {
        ILiquoriceSettlement.Order memory order = _order(amountIn, amountOut);
        return abi.encodeCall(
            ILiquoriceSettlement.settle,
            (
                maker,
                amountIn,
                order,
                new ILiquoriceSettlement.Interaction[](0),
                ILiquoriceSettlement.Hooks({
                    beforeSettle: new ILiquoriceSettlement.Interaction[](0),
                    afterSettle: new ILiquoriceSettlement.Interaction[](0)
                }),
                _sign(liquoriceSettlement.hashOrder(order)),
                _emptyTakerSignature()
            )
        );
    }

    function _runSwap(
        bytes memory liquoriceCalldata,
        uint256 amountIn,
        uint256 expectedAmountOut
    ) internal {
        _runSwap(
            liquoriceCalldata,
            0,
            amountIn,
            amountIn,
            amountIn,
            expectedAmountOut
        );
    }

    /// @dev `partialFillOffset` of 0 tells the executor the quote cannot be partially filled, so
    ///      it forwards the calldata unchanged.
    function _runSwap(
        bytes memory liquoriceCalldata,
        uint32 partialFillOffset,
        uint256 originalAmountIn,
        uint256 amountIn,
        uint256 minAmountIn,
        uint256 expectedAmountOut
    ) internal {
        deal(WETH_ADDR, maker, expectedAmountOut);
        deal(USDC_ADDR, address(liquoriceExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            partialFillOffset,
            originalAmountIn,
            minAmountIn,
            liquoriceCalldata
        );

        uint256 before = IERC20(WETH_ADDR).balanceOf(address(liquoriceExecutor));
        vm.prank(address(liquoriceExecutor));
        IERC20(USDC_ADDR).approve(LIQUORICE_BALANCE_MANAGER, amountIn);

        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));

        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(liquoriceExecutor)) - before,
            expectedAmountOut,
            "WETH should be at receiver"
        );
        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(liquoriceExecutor)),
            0,
            "USDC left in executor"
        );
    }

    function testSettleSingle() public {
        uint256 amountIn = 3000e6;
        uint256 amountOut = 1 ether;
        _runSwap(_encodeSettleSingle(amountIn, amountOut), amountIn, amountOut);
    }

    function testSettle() public {
        uint256 amountIn = 3000e6;
        uint256 amountOut = 1 ether;
        _runSwap(_encodeSettle(amountIn, amountOut), amountIn, amountOut);
    }

    function testDecodeData() public view {
        bytes memory liquoriceCalldata = abi.encodePacked(
            bytes4(0xdeadbeef),
            hex"1234567890abcdef1234567890abcdef"
            hex"1234567890abcdef1234567890abcdef"
        );

        uint256 originalAmount = 1000000000;
        uint256 minAmount = 800000000;

        bytes memory params = abi.encodePacked(
            USDC_ADDR, // tokenIn (20 bytes)
            WETH_ADDR, // tokenOut (20 bytes)
            uint32(5), // partialFillOffset (4 bytes)
            originalAmount, // originalBaseTokenAmount (32 bytes)
            minAmount, // minBaseTokenAmount (32 bytes)
            liquoriceCalldata // variable length
        );

        (
            address tokenIn,
            uint32 decodedPartialFillOffset,
            uint256 decodedOriginalAmount,
            uint256 decodedMinAmount,
            bytes memory decodedCalldata
        ) = liquoriceExecutor.decodeData(params);

        assertEq(tokenIn, USDC_ADDR);
        assertEq(decodedPartialFillOffset, 5, "partialFillOffset mismatch");
        assertEq(
            decodedOriginalAmount, originalAmount, "originalAmount mismatch"
        );
        assertEq(decodedMinAmount, minAmount, "minAmount mismatch");
        assertEq(
            keccak256(decodedCalldata),
            keccak256(liquoriceCalldata),
            "calldata mismatch"
        );
    }

    function testDecodeData_InvalidDataLength() public {
        bytes memory tooShort =
            abi.encodePacked(USDC_ADDR, WETH_ADDR, uint32(0));

        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__InvalidDataLength.selector
        );
        liquoriceExecutor.decodeData(tooShort);
    }

    function testInvalidSelector() public {
        bytes memory badCalldata = abi.encodePacked(
            bytes4(0xdeadbeef),
            hex"0000000000000000000000000000000000000000000000000000000000000000"
        );

        uint256 amountIn = 1000e6;
        bytes memory params = abi.encodePacked(
            USDC_ADDR, WETH_ADDR, uint32(0), amountIn, amountIn, badCalldata
        );

        deal(USDC_ADDR, address(liquoriceExecutor), amountIn);

        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__InvalidSelector.selector
        );
        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));
    }

    /// @dev `settle` on the settlement deployment preceding LIQUORICE_SETTLEMENT. Its calldata
    ///      is shaped for a different Order struct, so forwarding it would corrupt the trade.
    /// @dev The offsets are where `_filledTakerAmount` sits in each call's head: the fourth word
    ///      for `settleSingle`, the second for `settle`. The executor overwrites it in place, and
    ///      the settlement fills pro rata.
    function testSettleSingle_PartialFill() public {
        uint256 originalAmountIn = 3000e6;
        uint256 amountIn = 1500e6;
        _runSwap(
            _encodeSettleSingle(originalAmountIn, 1 ether),
            96,
            originalAmountIn,
            amountIn,
            amountIn,
            0.5 ether
        );
    }

    function testSettle_PartialFill() public {
        uint256 originalAmountIn = 3000e6;
        uint256 amountIn = 1500e6;
        _runSwap(
            _encodeSettle(originalAmountIn, 1 ether),
            32,
            originalAmountIn,
            amountIn,
            amountIn,
            0.5 ether
        );
    }

    function testRetiredSettleSelectorIsRejected() public {
        bytes memory retiredCalldata = abi.encodePacked(
            bytes4(0xcba673a7),
            hex"0000000000000000000000000000000000000000000000000000000000000000"
        );

        uint256 amountIn = 1000e6;
        bytes memory params = abi.encodePacked(
            USDC_ADDR, WETH_ADDR, uint32(0), amountIn, amountIn, retiredCalldata
        );

        deal(USDC_ADDR, address(liquoriceExecutor), amountIn);

        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__InvalidSelector.selector
        );
        liquoriceExecutor.swap(amountIn, params, address(liquoriceExecutor));
    }

    function testConstructor_NotAContract_Settlement() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__NotAContract.selector
        );
        new LiquoriceExecutorExposed(address(0x1), LIQUORICE_BALANCE_MANAGER);
    }

    function testConstructor_NotAContract_BalanceManager() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__NotAContract.selector
        );
        new LiquoriceExecutorExposed(LIQUORICE_SETTLEMENT, address(0x1));
    }

    function testClampAmount_WithinRange() public view {
        uint256 result = liquoriceExecutor.clampAmount(500, 1000, 100);
        assertEq(result, 500, "Should return givenAmount when within range");
    }

    function testClampAmount_ExceedsMax() public view {
        uint256 result = liquoriceExecutor.clampAmount(1500, 1000, 100);
        assertEq(
            result,
            1000,
            "Should clamp to originalBaseTokenAmount when exceeded"
        );
    }

    function testClampAmount_BelowMin_Reverts() public {
        vm.expectRevert(
            LiquoriceExecutor.LiquoriceExecutor__AmountBelowMinimum.selector
        );
        liquoriceExecutor.clampAmount(50, 1000, 100);
    }
}

contract TychoRouterForLiquoriceTest is TychoRouterTestSetup {
    using SafeERC20 for IERC20;

    address constant MAKER = 0x06465bcEEaef280Bb7340A58D75dfc5E1F687058;

    function getForkBlock() public pure override returns (uint256) {
        return 25900000;
    }

    function setUp() public override {
        super.setUp();

        ILiquoriceSettlement settlement =
            ILiquoriceSettlement(LIQUORICE_SETTLEMENT);
        address authenticator = settlement.AUTHENTICATOR();

        // The settlement gates callers and makers behind Liquorice's own allowlist, which
        // neither this router nor the fixture maker is on.
        vm.mockCall(
            authenticator,
            abi.encodeWithSelector(
                IAllowListAuthentication.isSolver.selector, address(tychoRouter)
            ),
            abi.encode(true)
        );
        vm.mockCall(
            authenticator,
            abi.encodeWithSelector(
                IAllowListAuthentication.isMaker.selector, MAKER
            ),
            abi.encode(true)
        );

        vm.prank(MAKER);
        IERC20(WETH_ADDR).approve(LIQUORICE_BALANCE_MANAGER, type(uint256).max);
    }

    function testSettleSingleLiquoriceIntegration() public {
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 3000e6);
        deal(WETH_ADDR, MAKER, 1 ether);
        uint256 expAmountOut = 1 ether;

        uint256 wethBefore = IERC20(WETH_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_liquorice_settle_single"
        );
        // Mock ecrecover precompile to return MAKER, bypassing order signature
        // verification. The test calldata uses a modified user address, so the
        // original signature no longer recovers to the correct maker.
        vm.mockCall(address(0x01), abi.encode(), abi.encode(MAKER));

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        uint256 wethReceived = IERC20(WETH_ADDR).balanceOf(user) - wethBefore;
        assertEq(wethReceived, expAmountOut, "Incorrect WETH received");
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "USDC left in router"
        );
        vm.stopPrank();
    }

    function testSettleLiquoriceIntegration() public {
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 3000e6);
        deal(WETH_ADDR, MAKER, 1 ether);
        uint256 expAmountOut = 1 ether;

        uint256 wethBefore = IERC20(WETH_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_liquorice_settle"
        );
        // Mock ecrecover precompile to return MAKER, bypassing order signature
        // verification. The test calldata uses a modified user address, so the
        // original signature no longer recovers to the correct maker.
        vm.mockCall(address(0x01), abi.encode(), abi.encode(MAKER));

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        uint256 wethReceived = IERC20(WETH_ADDR).balanceOf(user) - wethBefore;
        assertEq(wethReceived, expAmountOut, "Incorrect WETH received");
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "USDC left in router"
        );
        vm.stopPrank();
    }
}
