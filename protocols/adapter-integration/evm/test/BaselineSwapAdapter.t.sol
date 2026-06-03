// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "src/baseline/BaselineSwapAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

contract BaselineSwapAdapterTest is Test, ISwapAdapterTypes {
    address internal constant BTOKEN =
        address(0x1000000000000000000000000000000000000001);
    address internal constant RESERVE =
        address(0x2000000000000000000000000000000000000002);
    address internal constant OTHER =
        address(0x3000000000000000000000000000000000000003);

    BaselineSwapAdapter internal adapter;
    MockBaselineRelay internal relay;

    bytes32 internal constant POOL_ID = bytes32(bytes20(BTOKEN));

    function setUp() public {
        vm.etch(BTOKEN, address(new MockERC20()).code);
        vm.etch(RESERVE, address(new MockERC20()).code);
        vm.etch(OTHER, address(new MockERC20()).code);

        relay = new MockBaselineRelay();
        relay.setPool(BTOKEN, RESERVE);
        relay.setQuotes(BTOKEN, 42 ether, 2 ether, 5 ether, 3 ether);
        relay.setTotals(BTOKEN, 100 ether, 200 ether);

        adapter = new BaselineSwapAdapter(address(relay));
    }

    function testSwapBuysBTokenWithReserveExactIn() public {
        MockERC20(RESERVE).mint(address(this), 100 ether);
        MockERC20(RESERVE).approve(address(adapter), 100 ether);

        Trade memory trade =
            adapter.swap(POOL_ID, RESERVE, BTOKEN, OrderSide.Sell, 100 ether);

        assertEq(trade.calculatedAmount, 42 ether);
        assertEq(trade.price.numerator, 0);
        assertEq(trade.price.denominator, 1);
        assertEq(MockERC20(RESERVE).balanceOf(address(this)), 0);
        assertEq(MockERC20(RESERVE).balanceOf(address(relay)), 100 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(this)), 42 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(adapter)), 0);
    }

    function testSwapSellsBTokenForReserveExactIn() public {
        MockERC20(BTOKEN).mint(address(this), 10 ether);
        MockERC20(BTOKEN).approve(address(adapter), 10 ether);

        Trade memory trade =
            adapter.swap(POOL_ID, BTOKEN, RESERVE, OrderSide.Sell, 10 ether);

        assertEq(trade.calculatedAmount, 2 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(this)), 0);
        assertEq(MockERC20(BTOKEN).balanceOf(address(relay)), 10 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(this)), 2 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(adapter)), 0);
    }

    function testZeroAmountReturnsEmptyTrade() public {
        Trade memory trade =
            adapter.swap(POOL_ID, RESERVE, BTOKEN, OrderSide.Sell, 0);

        assertEq(trade.calculatedAmount, 0);
        assertEq(trade.gasUsed, 0);
        assertEq(trade.price.numerator, 0);
        assertEq(trade.price.denominator, 1);
    }

    function testSwapBuysBTokenWithReserveExactOut() public {
        MockERC20(RESERVE).mint(address(this), 100 ether);
        MockERC20(RESERVE).approve(address(adapter), 100 ether);

        Trade memory trade =
            adapter.swap(POOL_ID, RESERVE, BTOKEN, OrderSide.Buy, 12 ether);

        assertEq(trade.calculatedAmount, 5 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(this)), 95 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(relay)), 5 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(this)), 12 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(adapter)), 0);
    }

    function testSwapSellsBTokenForReserveExactOut() public {
        MockERC20(BTOKEN).mint(address(this), 100 ether);
        MockERC20(BTOKEN).approve(address(adapter), 100 ether);

        Trade memory trade =
            adapter.swap(POOL_ID, BTOKEN, RESERVE, OrderSide.Buy, 7 ether);

        assertEq(trade.calculatedAmount, 3 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(this)), 97 ether);
        assertEq(MockERC20(BTOKEN).balanceOf(address(relay)), 3 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(this)), 7 ether);
        assertEq(MockERC20(RESERVE).balanceOf(address(adapter)), 0);
    }

    function testInvalidTokenPairReverts() public {
        vm.expectRevert();
        adapter.swap(POOL_ID, RESERVE, OTHER, OrderSide.Sell, 1 ether);
    }

    function testNonCanonicalReservePairReverts() public {
        MockERC20(OTHER).mint(address(this), 1 ether);
        MockERC20(OTHER).approve(address(adapter), 1 ether);

        vm.expectRevert();
        adapter.swap(POOL_ID, OTHER, BTOKEN, OrderSide.Sell, 1 ether);
    }

    function testGetCapabilities() public {
        Capability[] memory capabilities =
            adapter.getCapabilities(POOL_ID, RESERVE, BTOKEN);

        assertEq(capabilities.length, 2);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));

        vm.expectRevert();
        adapter.getCapabilities(POOL_ID, OTHER, BTOKEN);
    }

    function testGetLimits() public {
        uint256[] memory limits = adapter.getLimits(POOL_ID, RESERVE, BTOKEN);

        assertEq(limits.length, 2);
        assertEq(limits[0], 10 ether);
        assertEq(limits[1], 20 ether);

        vm.expectRevert();
        adapter.getLimits(POOL_ID, OTHER, BTOKEN);
    }

    function testGetLimitsForSellDirection() public view {
        uint256[] memory limits = adapter.getLimits(POOL_ID, BTOKEN, RESERVE);

        assertEq(limits.length, 2);
        assertEq(limits[0], 20 ether);
        assertEq(limits[1], 10 ether);
    }

    function testUnimplementedHelpersRevert() public {
        uint256[] memory amounts = new uint256[](1);

        vm.expectRevert();
        adapter.price(POOL_ID, RESERVE, BTOKEN, amounts);

        vm.expectRevert();
        adapter.getTokens(POOL_ID);

        vm.expectRevert();
        adapter.getPoolIds(0, 1);
    }
}

contract BaselineSwapAdapterMainnetForkTest is Test, ISwapAdapterTypes {
    address internal constant RELAY =
        address(0xc81Fd894C0acE037d133aF4886550aC8133568E8);
    address internal constant MAINNET_BTOKEN =
        address(0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63);
    address internal constant WETH =
        address(0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2);

    uint256 internal constant FORK_BLOCK = 24_930_105;
    uint256 internal constant BUY_EXACT_IN_AMOUNT = 1e15;
    uint256 internal constant SELL_EXACT_IN_AMOUNT = 1e18;
    uint256 internal constant EXPECTED_BUY_EXACT_IN_OUT = 675625833670487764;
    uint256 internal constant EXPECTED_SELL_EXACT_IN_OUT = 1450991853685636;
    uint256 internal constant EXACT_OUT_AMOUNT = 1e15;

    BaselineSwapAdapter internal adapter;
    bytes32 internal constant POOL_ID = bytes32(bytes20(MAINNET_BTOKEN));

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        adapter = new BaselineSwapAdapter(RELAY);
    }

    function testBuyExactInMatchesMainnetQuote() public {
        (uint256 expectedAmountOut,,) = IBaselineRelayQuotes(RELAY)
            .quoteBuyExactIn(MAINNET_BTOKEN, BUY_EXACT_IN_AMOUNT);
        assertEq(expectedAmountOut, EXPECTED_BUY_EXACT_IN_OUT);

        deal(WETH, address(this), BUY_EXACT_IN_AMOUNT);
        IERC20(WETH).approve(address(adapter), BUY_EXACT_IN_AMOUNT);

        Trade memory trade = adapter.swap(
            POOL_ID, WETH, MAINNET_BTOKEN, OrderSide.Sell, BUY_EXACT_IN_AMOUNT
        );

        assertEq(trade.calculatedAmount, expectedAmountOut);
        assertGt(trade.gasUsed, 0);
    }

    function testSellExactInMatchesMainnetQuote() public {
        (uint256 expectedAmountOut,,) = IBaselineRelayQuotes(RELAY)
            .quoteSellExactIn(MAINNET_BTOKEN, SELL_EXACT_IN_AMOUNT);
        assertEq(expectedAmountOut, EXPECTED_SELL_EXACT_IN_OUT);

        deal(MAINNET_BTOKEN, address(this), SELL_EXACT_IN_AMOUNT);
        IERC20(MAINNET_BTOKEN).approve(address(adapter), SELL_EXACT_IN_AMOUNT);

        Trade memory trade = adapter.swap(
            POOL_ID, MAINNET_BTOKEN, WETH, OrderSide.Sell, SELL_EXACT_IN_AMOUNT
        );

        assertEq(trade.calculatedAmount, expectedAmountOut);
        assertGt(trade.gasUsed, 0);
    }

    function testBuyExactOutMatchesMainnetQuote() public {
        (uint256 expectedAmountIn,,) = IBaselineRelayQuotes(RELAY)
            .quoteBuyExactOut(MAINNET_BTOKEN, EXACT_OUT_AMOUNT);

        deal(WETH, address(this), expectedAmountIn);
        IERC20(WETH).approve(address(adapter), expectedAmountIn);

        Trade memory trade = adapter.swap(
            POOL_ID, WETH, MAINNET_BTOKEN, OrderSide.Buy, EXACT_OUT_AMOUNT
        );

        assertEq(trade.calculatedAmount, expectedAmountIn);
        assertGt(trade.gasUsed, 0);
    }

    function testSellExactOutMatchesMainnetQuote() public {
        (uint256 expectedAmountIn,,) = IBaselineRelayQuotes(RELAY)
            .quoteSellExactOut(MAINNET_BTOKEN, EXACT_OUT_AMOUNT);

        deal(MAINNET_BTOKEN, address(this), expectedAmountIn);
        IERC20(MAINNET_BTOKEN).approve(address(adapter), expectedAmountIn);

        Trade memory trade = adapter.swap(
            POOL_ID, MAINNET_BTOKEN, WETH, OrderSide.Buy, EXACT_OUT_AMOUNT
        );

        assertEq(trade.calculatedAmount, expectedAmountIn);
        assertGt(trade.gasUsed, 0);
    }
}

contract MockBaselineRelay {
    mapping(address => address) public reserveForBToken;
    mapping(address => uint256) public buyQuoteForBToken;
    mapping(address => uint256) public sellQuoteForBToken;
    mapping(address => uint256) public buyExactOutQuoteForBToken;
    mapping(address => uint256) public sellExactOutQuoteForBToken;
    mapping(address => uint256) public totalReservesForBToken;
    mapping(address => uint256) public totalBTokensForBToken;

    function setPool(address bToken, address reserveToken) external {
        reserveForBToken[bToken] = reserveToken;
    }

    function setQuotes(
        address bToken,
        uint256 buyQuote,
        uint256 sellQuote,
        uint256 buyExactOutQuote,
        uint256 sellExactOutQuote
    ) external {
        buyQuoteForBToken[bToken] = buyQuote;
        sellQuoteForBToken[bToken] = sellQuote;
        buyExactOutQuoteForBToken[bToken] = buyExactOutQuote;
        sellExactOutQuoteForBToken[bToken] = sellExactOutQuote;
    }

    function setTotals(
        address bToken,
        uint256 reserveTotal,
        uint256 bTokenTotal
    ) external {
        totalReservesForBToken[bToken] = reserveTotal;
        totalBTokensForBToken[bToken] = bTokenTotal;
    }

    function buyTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        address reserveToken = reserveForBToken[bToken];
        MockERC20(reserveToken).transferFrom(msg.sender, address(this), amountIn);

        amountOut = buyQuoteForBToken[bToken];
        MockERC20(bToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function buyTokensExactOut(address bToken, uint256 amountOut, uint256)
        external
        returns (uint256 amountIn, uint256 feesReceived)
    {
        address reserveToken = reserveForBToken[bToken];
        amountIn = buyExactOutQuoteForBToken[bToken];
        MockERC20(reserveToken).transferFrom(msg.sender, address(this), amountIn);

        MockERC20(bToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function quoteBuyExactOut(address bToken, uint256)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage)
    {
        amountIn = buyExactOutQuoteForBToken[bToken];
        feesReceived = 0;
        slippage = 0;
    }

    function quoteSellExactOut(address bToken, uint256)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage)
    {
        amountIn = sellExactOutQuoteForBToken[bToken];
        feesReceived = 0;
        slippage = 0;
    }

    function reserve(address bToken) external view returns (address) {
        return reserveForBToken[bToken];
    }

    function totalReserves(address bToken) external view returns (uint256) {
        return totalReservesForBToken[bToken];
    }

    function totalBTokens(address bToken) external view returns (uint256) {
        return totalBTokensForBToken[bToken];
    }

    function sellTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        address reserveToken = reserveForBToken[bToken];
        MockERC20(bToken).transferFrom(msg.sender, address(this), amountIn);

        amountOut = sellQuoteForBToken[bToken];
        MockERC20(reserveToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function sellTokensExactOut(address bToken, uint256 amountOut, uint256)
        external
        returns (uint256 amountIn, uint256 feesReceived)
    {
        address reserveToken = reserveForBToken[bToken];
        amountIn = sellExactOutQuoteForBToken[bToken];
        MockERC20(bToken).transferFrom(msg.sender, address(this), amountIn);

        MockERC20(reserveToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }
}

contract MockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount)
        external
        returns (bool)
    {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

interface IBaselineRelayQuotes {
    function quoteBuyExactIn(address bToken, uint256 amountIn)
        external
        view
        returns (uint256 amountOut, uint256 feesReceived, uint256 slippage);

    function quoteBuyExactOut(address bToken, uint256 amountOut)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage);

    function quoteSellExactIn(address bToken, uint256 amountIn)
        external
        view
        returns (uint256 amountOut, uint256 feesReceived, uint256 slippage);

    function quoteSellExactOut(address bToken, uint256 amountOut)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage);
}
