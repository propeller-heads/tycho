// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/tessera/TesseraSwapAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

contract TesseraSwapAdapterTest is AdapterTest {
    TesseraSwapAdapter adapter;

    address constant TESSERA_SWAP = 0x55555522005BcAE1c2424D474BfD5ed477749E3e;
    address constant TREASURY = 0x3dBE077e7986657E95e1CC50089f17a5a4AF0AaE;
    // Pair contracts (EIP-1967 proxies registered on the engine).
    address constant WETH_PAIR = 0xf524C1Bc1C64A2C99bc7eccf19EDe9a1d89d5a7C;
    address constant CBBTC_PAIR = 0xED57BacDc2a990B631F8817853935791C122c356;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    // A recent Base block where 7 books are live and the WETH book's max
    // quotable clip was ~139 WETH. Pinned so the tests are reproducible: the
    // venue reprices every block and its freshness gate zeroes quotes when
    // the fork's block env drifts from the posted state.
    uint256 constant FORK_BLOCK = 50548423;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        adapter = new TesseraSwapAdapter(TESSERA_SWAP);

        vm.label(address(adapter), "TesseraSwapAdapter");
        vm.label(TESSERA_SWAP, "TesseraSwap");
        vm.label(WETH_PAIR, "WETH/USDC pair");
        vm.label(CBBTC_PAIR, "cbBTC/USDC pair");
        vm.label(TREASURY, "Treasury");
        vm.label(WETH, "WETH");
        vm.label(CBBTC, "cbBTC");
        vm.label(USDC, "USDC");
    }

    function _wethPoolId() internal pure returns (bytes32) {
        return bytes32(bytes20(WETH_PAIR));
    }

    function testConstructorConfig() public view {
        assertEq(address(adapter.tesseraSwap()), TESSERA_SWAP);
    }

    function testGetPoolIdsIsNotImplemented() public {
        // No on-chain enumeration from tokens to pair contracts exists; pool
        // ids come from the substreams component ids.
        vm.expectRevert();
        adapter.getPoolIds(0, 10);
    }

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(_wethPoolId());
        assertEq(tokens.length, 2);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);

        address[] memory cbbtcTokens =
            adapter.getTokens(bytes32(bytes20(CBBTC_PAIR)));
        assertEq(cbbtcTokens[0], CBBTC);
        assertEq(cbbtcTokens[1], USDC);
    }

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(_wethPoolId(), WETH, USDC);

        assertEq(capabilities.length, 4);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));
        assertEq(uint256(capabilities[2]), uint256(Capability.PriceFunction));
        assertEq(uint256(capabilities[3]), uint256(Capability.HardLimits));
    }

    function testGetLimits() public view {
        uint256[] memory limits = adapter.getLimits(_wethPoolId(), WETH, USDC);
        assertEq(limits.length, 2);
        // The WETH book's sell-side cap was ~139 WETH at the fork block.
        assertGt(limits[0], 100 ether);
        assertLt(limits[0], 200 ether);
        assertGt(limits[1], 0);

        uint256[] memory reverseLimits =
            adapter.getLimits(_wethPoolId(), USDC, WETH);
        assertGt(reverseLimits[0], 0);
        assertGt(reverseLimits[1], 0);
    }

    function testQuoteAboveLimitIsZero() public view {
        uint256 sellLimit = adapter.getLimits(_wethPoolId(), WETH, USDC)[0];
        (, uint256 amountOut) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(WETH, USDC, int256(sellLimit * 2));
        assertEq(amountOut, 0);
    }

    function testPrice() public view {
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = 1 ether;
        amounts[1] = 10 ether;
        Fraction[] memory prices =
            adapter.price(_wethPoolId(), WETH, USDC, amounts);

        assertEq(prices.length, 2);
        // ~2,500 USDC per WETH at the fork block; sanity-band the quote.
        uint256 unit0 = (prices[0].numerator * 1 ether) / prices[0].denominator;
        assertGt(unit0, 1_000e6);
        assertLt(unit0, 10_000e6);
        // Near size-invariance: 10x the size moves the unit price < 0.1%.
        uint256 unit1 = (prices[1].numerator * 1 ether) / prices[1].denominator;
        assertApproxEqRel(unit0, unit1, 0.001e18);
    }

    function testSwapSellExactIn() public {
        uint256 amountIn = 1 ether;
        (, uint256 quoted) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(WETH, USDC, int256(amountIn));
        assertGt(quoted, 0);

        deal(WETH, address(this), amountIn);
        IERC20(WETH).approve(address(adapter), amountIn);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        Trade memory trade =
            adapter.swap(_wethPoolId(), WETH, USDC, OrderSide.Sell, amountIn);

        assertEq(trade.calculatedAmount, quoted);
        assertEq(IERC20(USDC).balanceOf(address(this)) - usdcBefore, quoted);
        assertGt(trade.gasUsed, 0);
    }

    function testSwapBuyExactOut() public {
        uint256 amountOut = 1_000e6; // 1,000 USDC
        (uint256 quotedIn,) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(WETH, USDC, -int256(amountOut));
        assertGt(quotedIn, 0);

        deal(WETH, address(this), quotedIn);
        IERC20(WETH).approve(address(adapter), quotedIn);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        Trade memory trade =
            adapter.swap(_wethPoolId(), WETH, USDC, OrderSide.Buy, amountOut);

        assertEq(trade.calculatedAmount, quotedIn);
        assertEq(IERC20(USDC).balanceOf(address(this)) - usdcBefore, amountOut);
    }

    function testSwapUsdcToWeth() public {
        uint256 amountIn = 2_500e6;
        (, uint256 quoted) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(USDC, WETH, int256(amountIn));
        assertGt(quoted, 0);

        deal(USDC, address(this), amountIn);
        IERC20(USDC).approve(address(adapter), amountIn);

        uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
        Trade memory trade =
            adapter.swap(_wethPoolId(), USDC, WETH, OrderSide.Sell, amountIn);

        assertEq(trade.calculatedAmount, quoted);
        assertEq(IERC20(WETH).balanceOf(address(this)) - wethBefore, quoted);
    }

    function testValidationRejectsWrongPool() public {
        // The cbBTC pair does not trade WETH.
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1 ether;

        vm.expectRevert();
        adapter.price(bytes32(bytes20(CBBTC_PAIR)), WETH, USDC, amounts);
    }

    function testValidationRejectsForeignToken() public {
        // cbBTC is not a token of the WETH/USDC pair.
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1 ether;

        vm.expectRevert();
        adapter.price(_wethPoolId(), WETH, CBBTC, amounts);
    }
}
