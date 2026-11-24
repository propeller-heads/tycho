// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {HanjiExecutor, IHanjiOrderbook} from "@src/executors/HanjiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

/// @dev The proxy orderbooks' whitelist, identified by its selectors on the unverified
///      implementations.
interface IHanjiWhitelist {
    function owner() external view returns (address);
    function setWhitelisted(address caller, bool allowed) external;
}

/// @dev An orderbook that reports an ERC20 market until the router approves it, and from then on
///      a market whose quote side is native ETH: the router's allowance is state the router
///      changes between `getTransferData` and `swap`, so a view can answer the two differently.
contract FlippingOrderbook {
    address public immutable base;
    address public immutable quote;

    constructor(address base_, address quote_) {
        base = base_;
        quote = quote_;
    }

    function getConfig()
        external
        view
        returns (
            uint256,
            uint256,
            address,
            address,
            bool,
            bool,
            address,
            address,
            uint64,
            uint64,
            uint64,
            uint64,
            bool
        )
    {
        bool approved =
            IERC20(quote).allowance(msg.sender, address(this)) > 0;
        return (
            1,
            1,
            base,
            quote,
            approved,
            false,
            address(0),
            address(0),
            0,
            0,
            0,
            0,
            false
        );
    }

    function placeMarketOrderWithTargetValue(
        bool,
        uint128,
        uint72,
        uint128,
        bool,
        uint256
    ) external payable returns (uint128, uint128, uint128) {
        return (1, 1, 0);
    }
}

/// @dev An ERC20 orderbook at scaling 1 whose buy reports the fill the test scripted, so the
///      executor's fill check can be driven to its boundary. It moves no tokens.
contract ScriptedOrderbook {
    address public immutable base;
    address public immutable quote;
    uint128 internal _shares;
    uint128 internal _value;
    uint128 internal _fee;

    constructor(address base_, address quote_) {
        base = base_;
        quote = quote_;
    }

    function script(uint128 shares, uint128 value, uint128 fee) external {
        (_shares, _value, _fee) = (shares, value, fee);
    }

    function getConfig()
        external
        view
        returns (
            uint256,
            uint256,
            address,
            address,
            bool,
            bool,
            address,
            address,
            uint64,
            uint64,
            uint64,
            uint64,
            bool
        )
    {
        return (
            1,
            1,
            base,
            quote,
            false,
            false,
            address(0),
            address(0),
            0,
            0,
            0,
            0,
            false
        );
    }

    function placeMarketOrderWithTargetValue(
        bool,
        uint128,
        uint72,
        uint128,
        bool,
        uint256
    ) external payable returns (uint128, uint128, uint128) {
        return (_shares, _value, _fee);
    }
}

contract HanjiTest is TychoRouterTestSetup {
    HanjiExecutor public hanjiExecutor;
    address constant ETH_USDC = 0x6A6951db3bb99f76A8cCB9f8A8399B613666298B;
    address constant CBBTC_USDC = 0x15C8cb4E336b1E478d155D813830129fb225eeE3;
    address constant CBBTC_ETH = 0xf455b3C0756908c6a2F44a7f7390053cFe354eeA;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    /// @dev Where the executor is placed: derived from its name, so deployments the shared
    ///      setup adds do not move it, and the Rust-encoded calldata
    ///      (config/test_executor_addresses.json) keeps addressing it.
    address constant HANJI_EXECUTOR =
        address(uint160(uint256(keccak256("tycho.test.HanjiExecutor"))));
    /// @dev The executor's price bounds, which any level clears.
    uint72 constant MIN_PRICE = 1;
    uint72 constant MAX_PRICE = 999_999e15;

    /// @dev What a direct fill on the orderbook took and paid out.
    struct Fill {
        uint256 spent;
        uint256 received;
    }

    function setUp() public override {
        super.setUp();
        deployCodeTo("HanjiExecutor.sol:HanjiExecutor", HANJI_EXECUTOR);
        hanjiExecutor = HanjiExecutor(payable(HANJI_EXECUTOR));
        address[] memory executors = new address[](1);
        executors[0] = HANJI_EXECUTOR;
        // Register before the pinned timestamp: the quoters' books empty about 3 s after their
        // keepers last pushed a price, so the swaps must run at the fork's own timestamp.
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        // The block the simulator's fixture live_61693481.json was recorded at.
        return 61693481;
    }

    /// @dev A native input is sent by the executor, an ERC20 one is debited by the orderbook, and
    ///      the output always lands at the router; the tokens are the data's.
    function testTransferData() public view {
        _assertTransferData(
            ETH_USDC,
            ETH_ADDRESS,
            USDC,
            TransferManager.TransferType.TransferNativeInExecutor
        );
        _assertTransferData(
            ETH_USDC,
            USDC,
            ETH_ADDRESS,
            TransferManager.TransferType.ProtocolWillDebit
        );
        _assertTransferData(
            CBBTC_ETH,
            CBBTC,
            ETH_ADDRESS,
            TransferManager.TransferType.ProtocolWillDebit
        );
        _assertTransferData(
            CBBTC_ETH,
            ETH_ADDRESS,
            CBBTC,
            TransferManager.TransferType.TransferNativeInExecutor
        );
        _assertTransferData(
            CBBTC_USDC,
            CBBTC,
            USDC,
            TransferManager.TransferType.ProtocolWillDebit
        );
        assertEq(hanjiExecutor.fundsExpectedAddress(""), address(this));
    }

    function _assertTransferData(
        address orderbook,
        address expectedIn,
        address expectedOut,
        TransferManager.TransferType expectedKind
    ) internal view {
        (
            TransferManager.TransferType kind,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = hanjiExecutor.getTransferData(
            abi.encodePacked(orderbook, expectedIn, expectedOut)
        );
        assertEq(uint8(kind), uint8(expectedKind));
        assertEq(receiver, orderbook);
        assertEq(tokenIn, expectedIn);
        assertEq(tokenOut, expectedOut);
        assertTrue(outputToRouter);
    }

    /// @dev Rejects truncation, extra bytes and the zero orderbook.
    function testInvalidData() public {
        bytes[4] memory invalid = [
            abi.encodePacked(ETH_USDC, uint8(1)),
            abi.encodePacked(ETH_USDC, ETH_ADDRESS),
            abi.encodePacked(ETH_USDC, ETH_ADDRESS, USDC, uint8(0)),
            abi.encodePacked(address(0), ETH_ADDRESS, USDC)
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(HanjiExecutor.HanjiExecutor__InvalidData.selector);
            hanjiExecutor.getTransferData(invalid[i]);
        }
    }

    /// @dev The data's pair must be the market's, with native ETH where the market has it: WETH
    ///      for ETH/USDC's native base, a token the market does not trade, or the same token on
    ///      both sides, are all refused.
    function testAPairOtherThanTheMarketsIsRefused() public {
        bytes[3] memory mismatched = [
            abi.encodePacked(ETH_USDC, WETH, USDC),
            abi.encodePacked(CBBTC_USDC, CBBTC, WETH),
            abi.encodePacked(CBBTC_USDC, USDC, USDC)
        ];
        for (uint256 i; i < mismatched.length; ++i) {
            vm.expectRevert(HanjiExecutor.HanjiExecutor__TokenMismatch.selector);
            hanjiExecutor.getTransferData(mismatched[i]);
        }
    }

    function testAZeroScalingConfigIsRefused() public {
        _assertZeroScalingIsRefused(0, 1);
        _assertZeroScalingIsRefused(1, 0);
    }

    function _assertZeroScalingIsRefused(uint256 scalingX, uint256 scalingY)
        internal
    {
        vm.mockCall(
            CBBTC_USDC,
            abi.encodeWithSelector(IHanjiOrderbook.getConfig.selector),
            abi.encode(
                scalingX,
                scalingY,
                CBBTC,
                USDC,
                false,
                false,
                address(0),
                address(0),
                uint64(0),
                uint64(0),
                uint64(0),
                uint64(0),
                false
            )
        );
        vm.expectRevert(HanjiExecutor.HanjiExecutor__InvalidConfig.selector);
        hanjiExecutor.getTransferData(abi.encodePacked(CBBTC_USDC, CBBTC, USDC));
    }

    /// @dev An input past uint128 shares is refused before anything is sent.
    function testAnInputPastUint128SharesIsRefused() public {
        vm.expectRevert(HanjiExecutor.HanjiExecutor__AmountTooLarge.selector);
        hanjiExecutor.swap(
            type(uint256).max,
            abi.encodePacked(CBBTC_USDC, CBBTC, USDC),
            address(0)
        );
    }

    /// @dev An orderbook that answers `getTransferData` with an ERC20 market and `swap` with a
    ///      native one cannot make the executor send the router's ETH: the data says the input
    ///      is an ERC20, and `swap` refuses the config that disagrees.
    function testAnOrderbookCannotTurnAnErc20InputIntoTheRoutersEth() public {
        FlippingOrderbook orderbook = new FlippingOrderbook(CBBTC, USDC);
        uint256 routerEth = 5 ether;
        deal(tychoRouterAddr, routerEth);
        uint256 amount = 1000e6;
        deal(USDC, BOB, amount);

        vm.startPrank(BOB);
        IERC20(USDC).approve(tychoRouterAddr, amount);
        vm.expectRevert(HanjiExecutor.HanjiExecutor__TokenMismatch.selector);
        tychoRouter.singleSwap(
            amount,
            USDC,
            CBBTC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                HANJI_EXECUTOR,
                abi.encodePacked(address(orderbook), USDC, CBBTC)
            )
        );
        vm.stopPrank();

        assertEq(tychoRouterAddr.balance, routerEth);
        assertEq(address(orderbook).balance, 0);
    }

    /// @dev Selling 1 ETH through the router fills exactly as a contract selling it on the
    ///      orderbook directly, with only the rounding remainder left at the router.
    function testRouterSellsEth() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_sell_eth",
            ETH_USDC,
            ETH_ADDRESS,
            USDC,
            1 ether
        );
    }

    /// @dev Buying ETH with 1,000 USDC: the router receives native ETH and forwards it.
    function testRouterBuysEth() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_buy_eth",
            ETH_USDC,
            USDC,
            ETH_ADDRESS,
            1000e6
        );
    }

    function testRouterSellsCbbtc() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_sell_cbbtc",
            CBBTC_USDC,
            CBBTC,
            USDC,
            1e6
        );
    }

    function testRouterBuysCbbtc() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_buy_cbbtc",
            CBBTC_USDC,
            USDC,
            CBBTC,
            1000e6
        );
    }

    /// @dev cbBTC/ETH has native ETH as its quote: selling cbBTC pays out native ETH.
    function testRouterSellsCbbtcForEth() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_sell_cbbtc_for_eth",
            CBBTC_ETH,
            CBBTC,
            ETH_ADDRESS,
            1e6
        );
    }

    /// @dev Buying cbBTC with native ETH sends it as `msg.value`; the orderbook refunds what it
    ///      does not spend to the router.
    function testRouterBuysCbbtcWithEth() public {
        _assertRouterFillsAsAContract(
            "test_single_encoding_strategy_hanji_buy_cbbtc_with_eth",
            CBBTC_ETH,
            ETH_ADDRESS,
            CBBTC,
            0.1 ether
        );
    }

    /// @dev The orderbook fills a contract that is not the transaction's sender, such as the
    ///      router, against a wider book than it fills the sender against.
    function testRouterIsFilledAsAContract() public {
        uint256 amount = 1 ether;
        Fill memory sender =
            _directFill(ETH_USDC, ETH_ADDRESS, USDC, amount, false);
        Fill memory contractCaller =
            _directFill(ETH_USDC, ETH_ADDRESS, USDC, amount, true);
        deal(BOB, amount);

        uint256 received = _routerSwap(
            "test_single_encoding_strategy_hanji_sell_eth",
            ETH_ADDRESS,
            USDC,
            amount
        );

        assertEq(received, contractCaller.received);
        assertLt(received, sender.received);
    }

    /// @dev Once the orderbook whitelists the router, it fills the router against the book it
    ///      fills the transaction's sender against.
    function testWhitelistedRouterIsFilledAsTheSender() public {
        uint256 amount = 1 ether;
        Fill memory sender =
            _directFill(ETH_USDC, ETH_ADDRESS, USDC, amount, false);
        vm.prank(IHanjiWhitelist(ETH_USDC).owner());
        IHanjiWhitelist(ETH_USDC).setWhitelisted(tychoRouterAddr, true);
        deal(BOB, amount);

        uint256 received = _routerSwap(
            "test_single_encoding_strategy_hanji_sell_eth",
            ETH_ADDRESS,
            USDC,
            amount
        );

        assertEq(received, sender.received);
    }

    /// @dev ETH -> USDC on one orderbook, then USDC -> cbBTC on another: the second hop spends
    ///      the first hop's output from the router, approved to its orderbook, and the route pays
    ///      exactly what the two fills pay a contract in a row.
    function testSequentialRouteThroughTwoMarkets() public {
        uint256 amount = 1 ether;
        uint256 expected = _chainedDirectFills(amount);
        deal(BOB, amount);

        uint256 received = _routerSwap(
            "test_sequential_encoding_strategy_hanji",
            ETH_ADDRESS,
            CBBTC,
            amount
        );

        assertEq(received, expected);
        assertEq(IERC20(USDC).allowance(tychoRouterAddr, CBBTC_USDC), 0);
    }

    /// @dev Once the quoters' prices are a few seconds old their book is empty. A sell then fills
    ///      nothing without the orderbook reverting, and the executor refuses it.
    function testASellIntoAStaleBookIsRefused() public {
        uint256 amount = 1e6;
        deal(CBBTC, BOB, amount);
        vm.warp(forkTimestamp + 10);

        vm.startPrank(BOB);
        IERC20(CBBTC).approve(tychoRouterAddr, amount);
        vm.expectRevert(HanjiExecutor.HanjiExecutor__NothingFilled.selector);
        tychoRouter.singleSwap(
            amount,
            CBBTC,
            USDC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                HANJI_EXECUTOR, abi.encodePacked(CBBTC_USDC, CBBTC, USDC)
            )
        );
        vm.stopPrank();
    }

    /// @dev A buy from a stale book is refused by the orderbook itself, which will not transfer
    ///      zero tokens (ZeroTokenTransferNotAllowed, 0x7c329980), before the executor checks it.
    function testABuyFromAStaleBookReverts() public {
        uint256 amount = 1000e6;
        deal(USDC, BOB, amount);
        vm.warp(forkTimestamp + 10);

        vm.startPrank(BOB);
        IERC20(USDC).approve(tychoRouterAddr, amount);
        vm.expectRevert(bytes4(0x7c329980));
        tychoRouter.singleSwap(
            amount,
            USDC,
            CBBTC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                HANJI_EXECUTOR, abi.encodePacked(CBBTC_USDC, USDC, CBBTC)
            )
        );
        vm.stopPrank();
    }

    /// @dev Selling more than the bids hold would leave the rest at the router: the executor
    ///      refuses it.
    function testASellPastTheBookIsRefused() public {
        uint256 amount = 1000e8;
        deal(CBBTC, BOB, amount);

        vm.startPrank(BOB);
        IERC20(CBBTC).approve(tychoRouterAddr, amount);
        vm.expectPartialRevert(
            HanjiExecutor.HanjiExecutor__PartialFill.selector
        );
        tychoRouter.singleSwap(
            amount,
            CBBTC,
            USDC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                HANJI_EXECUTOR, abi.encodePacked(CBBTC_USDC, CBBTC, USDC)
            )
        );
        vm.stopPrank();
    }

    /// @dev A buy may leave unspent less than two shares' cost at the fill's average price: 10
    ///      shares for 501 leave 100 unspent of 601 (under 2 x 50.1), and 10 shares for 500 leave
    ///      100 of 600, exactly two shares' cost, which is refused.
    function testABuyMayLeaveLessThanTwoSharesUnspent() public {
        ScriptedOrderbook orderbook = new ScriptedOrderbook(CBBTC, USDC);
        bytes memory data = abi.encodePacked(address(orderbook), USDC, CBBTC);

        orderbook.script(10, 501, 0);
        hanjiExecutor.swap(601, data, BOB);

        orderbook.script(10, 450, 50);
        vm.expectRevert(
            abi.encodeWithSelector(
                HanjiExecutor.HanjiExecutor__PartialFill.selector, 500, 600
            )
        );
        hanjiExecutor.swap(600, data, BOB);
    }

    /// @dev Buying with more than the asks cost would leave the rest at the router: the executor
    ///      refuses it.
    function testABuyPastTheBookIsRefused() public {
        uint256 amount = 100_000_000e6;
        deal(USDC, BOB, amount);

        vm.startPrank(BOB);
        IERC20(USDC).approve(tychoRouterAddr, amount);
        vm.expectPartialRevert(
            HanjiExecutor.HanjiExecutor__PartialFill.selector
        );
        tychoRouter.singleSwap(
            amount,
            USDC,
            CBBTC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                HANJI_EXECUTOR, abi.encodePacked(CBBTC_USDC, USDC, CBBTC)
            )
        );
        vm.stopPrank();
    }

    /// @dev The router swap through the Rust-encoded calldata `name` pays what a contract filling
    ///      `amount` on the orderbook directly receives, leaves at the router exactly the input
    ///      that fill did not spend, and leaves no allowance behind.
    function _assertRouterFillsAsAContract(
        string memory name,
        address orderbook,
        address input,
        address output,
        uint256 amount
    ) internal {
        Fill memory direct = _directFill(orderbook, input, output, amount, true);
        assertGt(direct.received, 0);
        if (input == ETH_ADDRESS) deal(BOB, amount);
        else deal(input, BOB, amount);

        uint256 received = _routerSwap(name, input, output, amount);

        assertEq(received, direct.received);
        assertEq(_balance(input, tychoRouterAddr), amount - direct.spent);
        if (input != ETH_ADDRESS) {
            assertEq(IERC20(input).allowance(tychoRouterAddr, orderbook), 0);
        }
    }

    /// @dev A router swap through the Rust-encoded calldata `name`, checking that BOB receives
    ///      what the router reports and that nothing of the output is left at the router.
    function _routerSwap(
        string memory name,
        address input,
        address output,
        uint256 amount
    ) internal returns (uint256 received) {
        uint256 before = _balance(output, BOB);
        vm.startPrank(BOB);
        uint256 value;
        if (input == ETH_ADDRESS) {
            value = amount;
        } else {
            IERC20(input).approve(tychoRouterAddr, amount);
        }
        (bool success, bytes memory result) =
            tychoRouterAddr.call{value: value}(loadCallDataFromFile(name));
        vm.stopPrank();
        assertTrue(success, "Rust-encoded Hanji swap failed");
        received = abi.decode(result, (uint256));
        assertEq(_balance(output, BOB) - before, received);
        assertEq(_balance(output, tychoRouterAddr), 0);
    }

    /// @dev What the orderbook fills for `amount` when called directly, measured on a snapshot
    ///      that is rolled back: the input it takes and the output it pays.
    function _directFill(
        address orderbook,
        address input,
        address output,
        uint256 amount,
        bool asContract
    ) internal returns (Fill memory fill) {
        uint256 snapshot = vm.snapshotState();
        fill = _fill(orderbook, input, output, amount, asContract);
        vm.revertToState(snapshot);
    }

    /// @dev What a contract receives selling `amount` ETH for USDC and then that USDC for cbBTC,
    ///      in a row, measured on a snapshot that is rolled back.
    function _chainedDirectFills(uint256 amount)
        internal
        returns (uint256 received)
    {
        uint256 snapshot = vm.snapshotState();
        Fill memory first = _fill(ETH_USDC, ETH_ADDRESS, USDC, amount, true);
        received = _fill(CBBTC_USDC, USDC, CBBTC, first.received, true).received;
        vm.revertToState(snapshot);
    }

    /// @dev Fills `amount` of `input` for `output` on the orderbook directly, from a fresh taker:
    ///      a contract that is not the transaction's sender when `asContract`, the sender itself
    ///      otherwise.
    function _fill(
        address orderbook,
        address input,
        address output,
        uint256 amount,
        bool asContract
    ) internal returns (Fill memory fill) {
        address taker = makeAddr(
            asContract ? "hanji contract taker" : "hanji sender taker"
        );
        uint256 value;
        if (input == ETH_ADDRESS) {
            deal(taker, amount);
            value = amount;
        } else {
            deal(input, taker, amount);
            vm.prank(taker);
            IERC20(input).approve(orderbook, amount);
        }
        uint256 inBefore = _balance(input, taker);
        uint256 outBefore = _balance(output, taker);
        (
            uint256 scalingX,
            uint256 scalingY,
            address tokenX,,
            bool supportsNativeEth,
            bool isTokenXWeth,,,,,,,
        ) = IHanjiOrderbook(orderbook).getConfig();
        bool sellBase = supportsNativeEth && isTokenXWeth
            ? input == ETH_ADDRESS
            : input == tokenX;
        // No test warps between a snapshot and its rollback, so the timestamp is the fill's.
        // forge-lint: disable-next-line(environment-read-across-mutation) Read at the fill's own timestamp.
        uint256 expires = block.timestamp + 1;
        if (asContract) {
            vm.etch(taker, hex"00");
            vm.prank(taker);
        } else {
            vm.prank(taker, taker);
        }
        if (sellBase) {
            IHanjiOrderbook(orderbook).placeOrder{value: value}(
                true,
                uint128(amount / scalingX),
                MIN_PRICE,
                type(uint128).max,
                true,
                false,
                true,
                expires
            );
        } else {
            IHanjiOrderbook(orderbook)
            .placeMarketOrderWithTargetValue{value: value}(
                false,
                uint128(amount / scalingY),
                MAX_PRICE,
                type(uint128).max,
                true,
                expires
            );
        }
        fill.spent = inBefore - _balance(input, taker);
        fill.received = _balance(output, taker) - outBefore;
    }

    function _balance(address token, address owner)
        internal
        view
        returns (uint256)
    {
        return token == ETH_ADDRESS
            ? owner.balance
            : IERC20(token).balanceOf(owner);
    }
}
