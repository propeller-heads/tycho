pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/BaselineExecutor.sol";
import {Constants} from "../Constants.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

contract BaselineExecutorExposed is BaselineExecutor {
    constructor(address relay_) BaselineExecutor(relay_) {}

    function decodeData(bytes calldata data) external pure returns (address bToken, address tokenIn, address tokenOut) {
        return _decodeData(data);
    }
}

contract BaselineExecutorTest is TestUtils, Constants {
    address internal constant BTOKEN = address(0x1000000000000000000000000000000000000001);
    address internal constant RESERVE = address(0x2000000000000000000000000000000000000002);
    address internal constant OTHER = address(0x3000000000000000000000000000000000000003);

    BaselineExecutorExposed executor;
    MockBaselineRelay relay;

    function setUp() public {
        vm.etch(BTOKEN, address(new MockERC20()).code);
        vm.etch(RESERVE, address(new MockERC20()).code);
        vm.etch(OTHER, address(new MockERC20()).code);

        relay = new MockBaselineRelay();
        relay.setReserve(BTOKEN, RESERVE);
        relay.setQuotes(BTOKEN, 42 ether, 2 ether);
        executor = new BaselineExecutorExposed(address(relay));
    }

    function testDecodeData() public view {
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE, BTOKEN);

        (address bToken, address tokenIn, address tokenOut) = executor.decodeData(data);

        assertEq(bToken, BTOKEN);
        assertEq(tokenIn, RESERVE);
        assertEq(tokenOut, BTOKEN);
    }

    function testDecodeDataInvalidLength() public {
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE);

        vm.expectRevert(BaselineExecutor__InvalidDataLength.selector);
        executor.decodeData(data);
    }

    function testConstructorZeroRelayReverts() public {
        vm.expectRevert(BaselineExecutor__ZeroAddress.selector);
        new BaselineExecutorExposed(address(0));
    }

    function testFundsExpectedAddressReturnsRouter() public {
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE, BTOKEN);
        address router = makeAddr("router");

        vm.prank(router);
        address receiver = executor.fundsExpectedAddress(data);

        assertEq(receiver, router);
    }

    function testGetTransferData() public view {
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE, BTOKEN);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.ProtocolWillDebit));
        assertEq(receiver, address(relay));
        assertEq(tokenIn, RESERVE);
        assertEq(tokenOut, BTOKEN);
        assertEq(outputToRouter, true);
    }

    function testGetTransferDataInvalidReserveReverts() public {
        bytes memory data = abi.encodePacked(BTOKEN, OTHER, BTOKEN);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.getTransferData(data);
    }

    function testBuyExactIn() public {
        uint256 amountIn = 100 ether;
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE, BTOKEN);

        MockERC20(RESERVE).mint(address(executor), amountIn);
        vm.prank(address(executor));
        MockERC20(RESERVE).approve(address(relay), amountIn);

        executor.swap(amountIn, data, BOB);

        assertEq(MockERC20(RESERVE).balanceOf(address(relay)), amountIn);
        assertEq(MockERC20(BTOKEN).balanceOf(address(executor)), 42 ether);
    }

    function testSellExactIn() public {
        uint256 amountIn = 10 ether;
        bytes memory data = abi.encodePacked(BTOKEN, BTOKEN, RESERVE);

        MockERC20(BTOKEN).mint(address(executor), amountIn);
        vm.prank(address(executor));
        MockERC20(BTOKEN).approve(address(relay), amountIn);

        executor.swap(amountIn, data, BOB);

        assertEq(MockERC20(BTOKEN).balanceOf(address(relay)), amountIn);
        assertEq(MockERC20(RESERVE).balanceOf(address(executor)), 2 ether);
    }

    function testInvalidTokenPairReverts() public {
        bytes memory data = abi.encodePacked(BTOKEN, RESERVE, OTHER);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.swap(1 ether, data, BOB);
    }

    function testInvalidBuyReserveReverts() public {
        bytes memory data = abi.encodePacked(BTOKEN, OTHER, BTOKEN);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.swap(1 ether, data, BOB);
    }

    function testUnknownBTokenReserveReverts() public {
        address unknownBToken = address(0x4000000000000000000000000000000000000004);
        bytes memory data = abi.encodePacked(unknownBToken, address(0), unknownBToken);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.swap(1 ether, data, BOB);
    }

    function testInvalidSellReserveReverts() public {
        bytes memory data = abi.encodePacked(BTOKEN, BTOKEN, OTHER);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.swap(1 ether, data, BOB);
    }

    function testSameBTokenPairReverts() public {
        bytes memory data = abi.encodePacked(BTOKEN, BTOKEN, BTOKEN);

        vm.expectRevert(BaselineExecutor__InvalidTokenPair.selector);
        executor.swap(1 ether, data, BOB);
    }
}

contract TychoRouterForBaselineTest is TychoRouterTestSetup {
    address internal constant BTOKEN = 0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63;
    uint256 internal constant BASELINE_BLOCK = 24_930_105;
    uint256 internal constant BUY_EXACT_IN_AMOUNT = 1e15;
    uint256 internal constant SELL_EXACT_IN_AMOUNT = 1e18;
    uint256 internal constant EXPECTED_BUY_EXACT_IN_OUT = 675625833670487764;
    uint256 internal constant EXPECTED_SELL_EXACT_IN_OUT = 1450991853685636;

    function getForkBlock() public pure override returns (uint256) {
        return BASELINE_BLOCK;
    }

    function testSingleBaselineBuyIntegration() public {
        deal(WETH_ADDR, ALICE, BUY_EXACT_IN_AMOUNT);
        uint256 balanceBefore = IERC20(BTOKEN).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile("test_single_encoding_strategy_baseline_buy");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(BTOKEN).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter - balanceBefore, EXPECTED_BUY_EXACT_IN_OUT);
        vm.stopPrank();
    }

    function testSingleBaselineSellIntegration() public {
        deal(BTOKEN, ALICE, SELL_EXACT_IN_AMOUNT);
        uint256 balanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BTOKEN).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile("test_single_encoding_strategy_baseline_sell");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(WETH_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BTOKEN).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter - balanceBefore, EXPECTED_SELL_EXACT_IN_OUT);
        vm.stopPrank();
    }
}

contract MockBaselineRelay {
    mapping(address => uint256) public buyQuoteForBToken;
    mapping(address => address) public reserveForBToken;
    mapping(address => uint256) public sellQuoteForBToken;

    function reserve(address bToken) external view returns (address) {
        return reserveForBToken[bToken];
    }

    function setReserve(address bToken, address reserve_) external {
        reserveForBToken[bToken] = reserve_;
    }

    function setQuotes(address bToken, uint256 buyQuote, uint256 sellQuote) external {
        buyQuoteForBToken[bToken] = buyQuote;
        sellQuoteForBToken[bToken] = sellQuote;
    }

    function buyTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        MockERC20(address(0x2000000000000000000000000000000000000002)).transferFrom(msg.sender, address(this), amountIn);

        amountOut = buyQuoteForBToken[bToken];
        MockERC20(bToken).mint(msg.sender, amountOut);
        feesReceived = 0;
    }

    function sellTokensExactIn(address bToken, uint256 amountIn, uint256)
        external
        returns (uint256 amountOut, uint256 feesReceived)
    {
        MockERC20(bToken).transferFrom(msg.sender, address(this), amountIn);

        amountOut = sellQuoteForBToken[bToken];
        MockERC20(address(0x2000000000000000000000000000000000000002)).mint(msg.sender, amountOut);
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

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 currentAllowance = allowance[from][msg.sender];
        if (currentAllowance != type(uint256).max) {
            allowance[from][msg.sender] = currentAllowance - amount;
        }
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}
