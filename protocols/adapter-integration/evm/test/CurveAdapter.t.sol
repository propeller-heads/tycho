// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "src/curve/CurveAdapter.sol";

contract CurveAdapterTest is Test, ISwapAdapterTypes, AdapterTest {
    using FractionMath for Fraction;

    CurveAdapter adapter;

    // tokens
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant ETH = address(0);
    address constant WBETH = 0xa2E3356610840701BDf5611a53974510Ae27E2e1;
    address constant MIM = 0x99D8a9C45b2ecA8864373A26D1459e3Dff1e17F3;
    address constant THREE_CRV_TOKEN =
        0x6c3F90f043a72FA612cbac8115EE7e52BDe6E490;
    address constant DAI = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant stETH = 0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84;
    address constant cUSDO = 0xaD55aebc9b8c03FC43cd9f62260391c13c23e7c0;
    // pools
    address constant STABLE_POOL = 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7;
    address constant CRYPTO_POOL = 0x80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5;
    address constant STABLE_META_POOL =
        0x5a6A4D54456819380173272A5E8E9B9904BdF41B;
    address constant ETH_POOL = 0xBfAb6FA95E0091ed66058ad493189D2cB29385E6;
    address constant STETH_POOL = 0xDC24316b9AE028F1497c275EB9192a3Ea0f67022;
    address[] ADDITIONAL_POOLS_FOR_TESTING;
    address constant ORACLE_STABLE_POOL =
        0x90455bd11Ce8a67C57d467e634Dc142b8e4105Aa;
    uint256 constant TEST_ITERATIONS = 100;
    IwstETH constant wstETH =
        IwstETH(0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0);

    function setUp() public {
        uint256 forkBlock = 23431163;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        adapter = new CurveAdapter();

        // Additional pools that include custom Int128 pools
        ADDITIONAL_POOLS_FOR_TESTING = [
            0xEcd5e75AFb02eFa118AF914515D6521aaBd189F1,
            0xEd279fDD11cA84bEef15AF5D39BB4d4bEE23F0cA,
            0x43b4FdFD4Ff969587185cDB6f0BD875c5Fc83f8c,
            0x9EfE1A1Cbd6Ca51Ee8319AFc4573d253C3B732af,
            0x4807862AA8b2bF68830e4C8dc86D0e9A998e085a,
            0xd632f22692FaC7611d2AA1C0D552930D43CAEd3B,
            0xA5407eAE9Ba41422680e2e00537571bcC53efBfD,
            0x5a6A4D54456819380173272A5E8E9B9904BdF41B,
            0x3211C6cBeF1429da3D0d58494938299C92Ad5860,
            0xDB6925eA42897ca786a045B252D95aA7370f44b4,
            0xf861483fa7E511fbc37487D91B6FAa803aF5d37c,
            0x1e098B32944292969fB58c85bDC85545DA397117,
            0xe0e970a99bc4F53804D8145beBBc7eBc9422Ba7F,
            0x6bfE880Ed1d639bF80167b93cc9c56a39C1Ba2dC,
            0xBDFAe7D2cF2E69E27b75a5287ECD3808F62B5a76,
            0xfB8814D005C5f32874391e888da6eB2fE7a27902,
            0x0f3159811670c117c372428D4E69AC32325e4D0F,
            0x4eBdF703948ddCEA3B11f675B4D1Fba9d2414A14,
            0x0E9B5B092caD6F1c5E6bc7f89Ffe1abb5c95F1C2,
            0x21410232B484136404911780bC32756D5d1a9Fa9,
            0x322135Dd9cBAE8Afa84727d9aE1434b5B3EBA44B,
            0xC26b89A667578ec7b3f11b2F98d6Fd15C07C54ba,
            0x9409280DC1e6D33AB7A8C6EC03e5763FB61772B5,
            0x5FAE7E604FC3e24fd43A72867ceBaC94c65b404A
        ];

        vm.label(address(adapter), "CurveAdapter");
        vm.label(USDT, "USDT");
        vm.label(USDC, "USDC");
        vm.label(STABLE_POOL, "STABLE_POOL");
        vm.label(WETH, "WETH");
        vm.label(CRYPTO_POOL, "CRYPTO_POOL");
    }

    receive() external payable {}

    function testSwapsForAdditionalPools() public {
        uint256 len = ADDITIONAL_POOLS_FOR_TESTING.length;
        for (uint256 i = 0; i < len; i++) {
            bytes32 pair = bytes32(bytes20(ADDITIONAL_POOLS_FOR_TESTING[i]));
            address[] memory tokens = adapter.getTokens(pair);
            uint256[] memory amounts = new uint256[](1);

            try ICurveStableSwapPool(ADDITIONAL_POOLS_FOR_TESTING[i])
                .balances(0) returns (
                uint256 bal
            ) {
                amounts[0] = bal / 10;
            } catch {
                amounts[0] = ICurveCustomInt128Pool(
                        ADDITIONAL_POOLS_FOR_TESTING[i]
                    )
                    .balances(int128(0)) / 10;
            }

            deal(tokens[0], address(this), amounts[0]);
            IERC20(tokens[0]).approve(address(adapter), amounts[0]);

            // Test Swap
            Trade memory trade = adapter.swap(
                pair,
                tokens[0],
                tokens[1],
                ISwapAdapterTypes.OrderSide.Sell,
                amounts[0]
            );

            // Test Limits
            uint256[] memory limits =
                adapter.getLimits(pair, tokens[0], tokens[1]);

            assertGt(trade.calculatedAmount, 0);
            assertGt(trade.price.numerator, 0);
            assertGt(trade.price.denominator, 0);
            assertGt(limits[0], 0);
            assertGt(limits[1], 0);
        }
    }

    function testSwapFuzzCurveStEthPool(
        uint256 specifiedAmount,
        bool invertedSides
    ) public {
        (address sellToken, address buyToken) =
            !invertedSides ? (ETH, stETH) : (stETH, ETH);
        (uint256 sellTokenBalBefore, uint256 buyTokenBalBefore) = (0, 0);

        bytes32 pair = bytes32(bytes20(STETH_POOL));
        uint256[] memory limits = adapter.getLimits(pair, sellToken, buyToken);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 5);

        if (sellToken == ETH) {
            deal(address(adapter), specifiedAmount);
            sellTokenBalBefore = address(adapter).balance;
            buyTokenBalBefore = IERC20(buyToken).balanceOf(address(this));
        } else {
            dealStEthTokens(specifiedAmount);
            IERC20(sellToken).approve(address(adapter), specifiedAmount);
            sellTokenBalBefore = IERC20(sellToken).balanceOf(address(this));
            buyTokenBalBefore = address(this).balance;
        }

        Trade memory trade = adapter.swap(
            pair, sellToken, buyToken, OrderSide.Sell, specifiedAmount
        );

        if (sellToken == ETH) {
            assertEq(
                specifiedAmount, sellTokenBalBefore - address(adapter).balance
            );
            assertGe(
                trade.calculatedAmount + 3,
                IERC20(buyToken).balanceOf(address(this)) - buyTokenBalBefore
            );
            assertLe(
                trade.calculatedAmount - 3,
                IERC20(buyToken).balanceOf(address(this)) - buyTokenBalBefore
            );
        } else {
            assertGe(
                specifiedAmount + 3,
                sellTokenBalBefore - IERC20(sellToken).balanceOf(address(this))
            );
            assertLe(
                specifiedAmount - 3,
                sellTokenBalBefore - IERC20(sellToken).balanceOf(address(this))
            );
            assertEq(
                trade.calculatedAmount,
                address(this).balance - buyTokenBalBefore
            );
        }
    }

    function testSwapFuzzCurveStableSwap(uint256 specifiedAmount) public {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(STABLE_POOL));
        uint256[] memory limits = adapter.getLimits(pair, USDC, USDT);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 4);

        deal(USDC, address(this), specifiedAmount);
        IERC20(USDC).approve(address(adapter), specifiedAmount);

        uint256 usdc_balance = IERC20(USDC).balanceOf(address(this));
        uint256 USDT_balance = IERC20(USDT).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, USDC, USDT, side, specifiedAmount);

        assertEq(
            specifiedAmount,
            usdc_balance - IERC20(USDC).balanceOf(address(this))
        );
        assertEq(
            trade.calculatedAmount,
            IERC20(USDT).balanceOf(address(this)) - USDT_balance
        );
    }

    function testSwapFuzzCurveStableOracleSwap(uint256 specifiedAmount) public {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(ORACLE_STABLE_POOL));
        uint256[] memory limits = adapter.getLimits(pair, cUSDO, USDC);

        specifiedAmount = bound(specifiedAmount, 1e18 + 1, limits[0] - 1);

        deal(cUSDO, address(this), specifiedAmount);
        IERC20(cUSDO).approve(address(adapter), specifiedAmount);

        uint256 usdc_balance = IERC20(USDC).balanceOf(address(this));
        uint256 cusdo_balance = IERC20(cUSDO).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, cUSDO, USDC, side, specifiedAmount);

        assertEq(
            specifiedAmount,
            cusdo_balance - IERC20(cUSDO).balanceOf(address(this))
        );
        assertEq(
            trade.calculatedAmount,
            IERC20(USDC).balanceOf(address(this)) - usdc_balance
        );
    }

    function testSwapFuzzCurveCryptoSwap(uint256 specifiedAmount) public {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));
        uint256[] memory limits = adapter.getLimits(pair, WETH, USDT);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 6);

        deal(WETH, address(this), specifiedAmount);
        IERC20(WETH).approve(address(adapter), specifiedAmount);

        uint256 WETH_balance = IERC20(WETH).balanceOf(address(this));
        uint256 USDT_balance = IERC20(USDT).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, WETH, USDT, side, specifiedAmount);

        assertEq(
            specifiedAmount,
            WETH_balance - IERC20(WETH).balanceOf(address(this))
        );
        assertEq(
            trade.calculatedAmount,
            IERC20(USDT).balanceOf(address(this)) - USDT_balance
        );
    }

    function testSwapFuzzCurveCryptoSwapUsingEth(uint256 specifiedAmount)
        public
    {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));
        uint256[] memory limits = adapter.getLimits(pair, ETH, USDT);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 6);

        deal(address(adapter), specifiedAmount);

        uint256 ETH_balance = address(adapter).balance;
        uint256 USDT_balance = IERC20(USDT).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, ETH, USDT, side, specifiedAmount);

        assertEq(specifiedAmount, ETH_balance - address(adapter).balance);
        assertEq(
            trade.calculatedAmount,
            IERC20(USDT).balanceOf(address(this)) - USDT_balance
        );
    }

    function testSwapFuzzCurveStablePoolEthWithEth(uint256 specifiedAmount)
        public
    {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(ETH_POOL));
        uint256[] memory limits = adapter.getLimits(pair, ETH, WBETH);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 14);

        deal(address(adapter), specifiedAmount);

        uint256 eth_balance = address(adapter).balance;
        uint256 WBETH_balance = IERC20(WBETH).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, ETH, WBETH, side, specifiedAmount);

        assertEq(specifiedAmount, eth_balance - address(adapter).balance);
        assertEq(
            trade.calculatedAmount,
            IERC20(WBETH).balanceOf(address(this)) - WBETH_balance
        );
    }

    function testSwapFuzzCurveStablePoolEthWithToken(uint256 specifiedAmount)
        public
    {
        OrderSide side = OrderSide.Sell;

        bytes32 pair = bytes32(bytes20(ETH_POOL));
        uint256[] memory limits = adapter.getLimits(pair, WBETH, ETH);

        vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10 ** 14);

        deal(address(WBETH), address(this), specifiedAmount);
        IERC20(WBETH).approve(address(adapter), specifiedAmount);

        uint256 eth_balance = address(this).balance;
        uint256 WBETH_balance = IERC20(WBETH).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, WBETH, ETH, side, specifiedAmount);

        assertEq(trade.calculatedAmount, address(this).balance - eth_balance);
        assertEq(
            specifiedAmount,
            WBETH_balance - IERC20(WBETH).balanceOf(address(this))
        );
    }

    function testSwapSellIncreasingSwapsCurve() public {
        executeIncreasingSwapsStableSwap(OrderSide.Sell);
        executeIncreasingSwapsCryptoSwap(OrderSide.Sell);
    }

    function executeIncreasingSwapsStableSwap(OrderSide side) internal {
        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));

        uint256[] memory amounts = new uint256[](TEST_ITERATIONS);
        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            amounts[i] = 1000 * i * 10 ** 14;
        }

        Trade[] memory trades = new Trade[](TEST_ITERATIONS);
        uint256 beforeSwap;
        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            beforeSwap = vm.snapshot();

            deal(WETH, address(this), amounts[i]);
            IERC20(WETH).approve(address(adapter), amounts[i]);

            trades[i] = adapter.swap(pair, WETH, USDT, side, amounts[i]);
            vm.revertTo(beforeSwap);
        }

        for (uint256 i = 1; i < TEST_ITERATIONS - 1; i++) {
            assertLe(trades[i].calculatedAmount, trades[i + 1].calculatedAmount);
            assertEq(trades[i].price.compareFractions(trades[i + 1].price), 1);
        }
    }

    function executeIncreasingSwapsCryptoSwap(OrderSide side) internal {
        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));

        uint256[] memory amounts = new uint256[](TEST_ITERATIONS);
        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            amounts[i] = 1000 * i * 10 ** 6;
        }

        Trade[] memory trades = new Trade[](TEST_ITERATIONS);
        uint256 beforeSwap;
        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            beforeSwap = vm.snapshot();

            deal(WETH, address(this), amounts[i]);
            IERC20(WETH).approve(address(adapter), amounts[i]);

            trades[i] = adapter.swap(pair, WETH, USDT, side, amounts[i]);
            vm.revertTo(beforeSwap);
        }

        for (uint256 i = 1; i < TEST_ITERATIONS - 1; i++) {
            assertLe(trades[i].calculatedAmount, trades[i + 1].calculatedAmount);
            assertEq(trades[i].price.compareFractions(trades[i + 1].price), 1);
        }
    }

    function testGetCapabilitiesCurveSwap(bytes32 pair, address t0, address t1)
        public
    {
        Capability[] memory res = adapter.getCapabilities(pair, t0, t1);

        assertEq(res.length, 1);
    }

    function testGetTokensCurveStableSwap() public {
        bytes32 pair = bytes32(bytes20(STABLE_POOL));
        address[] memory tokens = adapter.getTokens(pair);

        assertGe(tokens.length, 2);
    }

    function testGetTokensCurveCryptoSwap() public {
        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));
        address[] memory tokens = adapter.getTokens(pair);

        assertGe(tokens.length, 2);
    }

    function testGetLimitsCurveStableSwap() public {
        bytes32 pair = bytes32(bytes20(STABLE_POOL));
        uint256[] memory limits = adapter.getLimits(pair, USDC, USDT);

        assertEq(limits.length, 2);
    }

    function testGetLimitsCurveCryptoSwap() public {
        bytes32 pair = bytes32(bytes20(CRYPTO_POOL));
        uint256[] memory limits = adapter.getLimits(pair, WETH, USDT);

        assertEq(limits.length, 2);
    }

    /// @dev custom function to 'deal' stETH tokens as normal deal won't work
    function dealStEthTokens(uint256 amount) internal {
        uint256 wstETHAmount = wstETH.getStETHByWstETH(amount);
        deal(address(wstETH), address(this), wstETHAmount);
        wstETH.unwrap(wstETHAmount);
    }
}

interface IwstETH is IERC20 {
    function unwrap(uint256 _wstETHAmount) external returns (uint256);
    function getStETHByWstETH(uint256 _wstETHAmount)
        external
        view
        returns (uint256);
}
