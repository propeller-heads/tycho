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

        adapter = new BaselineSwapAdapter(address(relay));
    }

    function testSwapBuysBTokenWithReserveExactIn() public {
        MockERC20(RESERVE).mint(address(this), 100 ether);
        MockERC20(RESERVE).approve(address(adapter), 100 ether);

        Trade memory trade =
            adapter.swap(POOL_ID, RESERVE, BTOKEN, OrderSide.Sell, 100 ether);

        assertEq(trade.calculatedAmount, 42 ether);
        assertEq(trade.price.numerator, 0);
        assertEq(trade.price.denominator, 0);
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
        assertEq(trade.price.denominator, 0);
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

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(POOL_ID, RESERVE, BTOKEN);

        assertEq(capabilities.length, 2);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));
    }

    function testGetLimits() public view {
        uint256[] memory limits = adapter.getLimits(POOL_ID, RESERVE, BTOKEN);

        assertEq(limits.length, 2);
        assertEq(limits[0], type(uint256).max);
        assertEq(limits[1], type(uint256).max);
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

contract MockBaselineRelay {
    mapping(address => address) public reserveForBToken;
    mapping(address => uint256) public buyQuoteForBToken;
    mapping(address => uint256) public sellQuoteForBToken;
    mapping(address => uint256) public buyExactOutQuoteForBToken;
    mapping(address => uint256) public sellExactOutQuoteForBToken;

    function setPool(address bToken, address reserve) external {
        reserveForBToken[bToken] = reserve;
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

    function buyTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        address reserve = reserveForBToken[bToken];
        MockERC20(reserve).transferFrom(msg.sender, address(this), amountIn);

        amountOut = buyQuoteForBToken[bToken];
        MockERC20(bToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function buyTokensExactOut(address bToken, uint256 amountOut, uint256)
        external
        returns (uint256 amountIn, uint256 feesReceived)
    {
        address reserve = reserveForBToken[bToken];
        amountIn = buyExactOutQuoteForBToken[bToken];
        MockERC20(reserve).transferFrom(msg.sender, address(this), amountIn);

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

    function sellTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        address reserve = reserveForBToken[bToken];
        MockERC20(bToken).transferFrom(msg.sender, address(this), amountIn);

        amountOut = sellQuoteForBToken[bToken];
        MockERC20(reserve).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function sellTokensExactOut(address bToken, uint256 amountOut, uint256)
        external
        returns (uint256 amountIn, uint256 feesReceived)
    {
        address reserve = reserveForBToken[bToken];
        amountIn = sellExactOutQuoteForBToken[bToken];
        MockERC20(bToken).transferFrom(msg.sender, address(this), amountIn);

        MockERC20(reserve).mint(msg.sender, amountOut);
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
