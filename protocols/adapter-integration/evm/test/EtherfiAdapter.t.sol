// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/interfaces/ISwapAdapterTypes.sol";
import "src/libraries/FractionMath.sol";
import "src/etherfi/EtherfiAdapter.sol";

contract EtherfiAdapterTest is Test, ISwapAdapterTypes {
    using FractionMath for Fraction;

    EtherfiAdapter adapter;
    IWeEth weEth = IWeEth(0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee);
    IeEth eEth;

    uint256 constant TEST_ITERATIONS = 100;

    function setUp() public {
        uint256 forkBlock = 19218495;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        adapter = new EtherfiAdapter(address(weEth));
        eEth = weEth.eETH();

        vm.label(address(weEth), "WeETH");
        vm.label(address(eEth), "eETH");
    }

    receive() external payable {}

    function testPriceFuzzEtherfi(uint256 amount0, uint256 amount1)
        public
        view
    {
        bytes32 pair = bytes32(0);
        uint256[] memory limits = adapter.getLimits(
            pair, address(address(weEth)), address(address(eEth))
        );
        vm.assume(amount0 < limits[0] && amount0 > 0);
        vm.assume(amount1 < limits[1] && amount1 > 0);

        uint256[] memory amounts = new uint256[](2);
        amounts[0] = amount0;
        amounts[1] = amount1;

        Fraction[] memory prices = adapter.price(
            pair, address(address(weEth)), address(address(eEth)), amounts
        );

        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
        }
    }

    function testSwapFuzzEtherfiEethWeEth(uint256 specifiedAmount, bool isBuy)
        public
    {
        OrderSide side = isBuy ? OrderSide.Buy : OrderSide.Sell;

        IERC20 eEth_ = IERC20(address(eEth));
        IERC20 weEth_ = IERC20(address(weEth));
        bytes32 pair = bytes32(0);
        uint256[] memory limits =
            adapter.getLimits(pair, address(eEth_), address(weEth_));

        if (side == OrderSide.Buy) {
            // Bound specifiedAmount before computing requiredEeth to avoid
            // overflow in getEETHByWeETH (which multiplies by
            // totalPooledEther). Also ensure the required eETH input fits
            // within what we can fund:
            // weETH carries accumulated rewards so 1 weETH > 1 eETH in value,
            // meaning requiredEeth > specifiedAmount and the original limits[1]
            // bound was too loose.
            vm.assume(specifiedAmount < limits[1] && specifiedAmount > 100);
            uint256 requiredEeth = weEth.getEETHByWeETH(specifiedAmount);
            /// Request extra eETH to cover share-based rounding
            /// losses during transfer.
            uint256 fundingAmount = requiredEeth + 10;
            vm.assume(fundingAmount < limits[0]);

            /// @dev workaround for eETH "deal", as standard ERC20 does not
            /// work(balance is shares).
            deal(address(adapter), type(uint256).max);
            adapter.swap(
                pair,
                address(address(0)),
                address(eEth_),
                OrderSide.Buy,
                fundingAmount
            );

            eEth_.approve(address(adapter), type(uint256).max);
        } else {
            vm.assume(specifiedAmount < limits[0] && specifiedAmount > 100);

            /// @dev workaround for eETH "deal", as standard ERC20 does not
            /// work(balance is shares)
            deal(address(adapter), type(uint128).max);
            adapter.swap(
                pair,
                address(address(0)),
                address(eEth_),
                OrderSide.Buy,
                specifiedAmount
            );

            eEth_.approve(address(adapter), specifiedAmount);
        }

        uint256 eEth_balance = eEth_.balanceOf(address(this));
        uint256 weEth_balance = weEth_.balanceOf(address(this));

        Trade memory trade = adapter.swap(
            pair, address(eEth_), address(weEth_), side, specifiedAmount
        );

        if (trade.calculatedAmount > 0) {
            if (side == OrderSide.Buy) {
                assertGe(
                    weEth_.balanceOf(address(this)) - weEth_balance,
                    specifiedAmount - 2
                );
                /// @dev Transfer function contains rounding errors because of
                /// rewards in weETH contract, therefore we assume a +/-2
                /// tolerance
                assertLe(
                    weEth_.balanceOf(address(this)) - weEth_balance,
                    specifiedAmount
                );
                assertLe(
                    eEth_balance - eEth_.balanceOf(address(this)),
                    trade.calculatedAmount + 2
                );
                assertGe(
                    eEth_balance - eEth_.balanceOf(address(this)),
                    trade.calculatedAmount - 2
                );
            } else {
                assertGe(
                    specifiedAmount,
                    eEth_balance - eEth_.balanceOf(address(this))
                );
                /// @dev Transfer function contains rounding errors because of
                /// rewards in eETH contract, therefore we assume a +/-2
                /// tolerance
                assertLe(
                    specifiedAmount - 2,
                    eEth_balance - eEth_.balanceOf(address(this))
                );
                assertEq(
                    trade.calculatedAmount,
                    weEth_.balanceOf(address(this)) - weEth_balance
                );
            }
        }
    }

    function testSwapFuzzEtherfiWeEthEeth(uint256 specifiedAmount, bool isBuy)
        public
    {
        OrderSide side = isBuy ? OrderSide.Buy : OrderSide.Sell;

        IERC20 eEth_ = IERC20(address(eEth));
        IERC20 weEth_ = IERC20(address(weEth));
        uint256 weEth_bal_before = weEth_.balanceOf(address(this));
        bytes32 pair = bytes32(0);
        uint256[] memory limits =
            adapter.getLimits(pair, address(weEth_), address(eEth_));

        if (side == OrderSide.Buy) {
            vm.assume(specifiedAmount < limits[1] && specifiedAmount > 100);

            /// @dev workaround for eETH "deal", as standard ERC20 does not
            /// work(balance is shares)
            deal(address(adapter), type(uint256).max);
            adapter.swap(
                pair,
                address(address(0)),
                address(weEth_),
                OrderSide.Buy,
                limits[0]
            );

            weEth_.approve(address(adapter), type(uint256).max);
        } else {
            vm.assume(specifiedAmount < limits[0] && specifiedAmount > 100);

            /// @dev workaround for eETH "deal", as standard ERC20 does not
            /// work(balance is shares)
            deal(address(adapter), type(uint128).max);
            adapter.swap(
                pair,
                address(address(0)),
                address(weEth_),
                OrderSide.Buy,
                specifiedAmount
            );

            weEth_.approve(address(adapter), specifiedAmount);
        }

        uint256 eEth_balance = eEth_.balanceOf(address(this));
        uint256 weEth_balance = weEth_.balanceOf(address(this));

        /// @dev as of rounding errors in Etherfi, specifiedAmount might lose
        /// small digits for small numbers
        /// therefore we use weEth_balance - weEth_bal_before as specifiedAmount
        uint256 realAmountWeEth_ = weEth_balance - weEth_bal_before;

        Trade memory trade = adapter.swap(
            pair, address(weEth_), address(eEth_), side, realAmountWeEth_
        );

        if (trade.calculatedAmount > 0) {
            if (side == OrderSide.Buy) {
                assertGe(
                    realAmountWeEth_,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
                /// @dev Transfer function contains rounding errors because of
                /// rewards in weETH contract, therefore we assume a +/-2
                /// tolerance
                assertLe(
                    realAmountWeEth_ - 2,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
                assertLe(
                    trade.calculatedAmount - 2,
                    weEth_balance - weEth_.balanceOf(address(this))
                );
            } else {
                assertEq(
                    realAmountWeEth_,
                    weEth_balance - weEth_.balanceOf(address(this))
                );
                assertLe(
                    trade.calculatedAmount - 2,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
                assertGe(
                    trade.calculatedAmount,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
            }
        }
    }

    function testSwapFuzzEtherfiEthEeth(uint256 specifiedAmount, bool isBuy)
        public
    {
        OrderSide side = isBuy ? OrderSide.Buy : OrderSide.Sell;

        address eth_ = address(0);
        IERC20 eEth_ = IERC20(address(eEth));
        bytes32 pair = bytes32(0);
        uint256[] memory limits = adapter.getLimits(pair, eth_, address(eEth_));

        if (side == OrderSide.Buy) {
            vm.assume(specifiedAmount < limits[1] && specifiedAmount > 10);

            deal(address(adapter), eEth_.totalSupply());
        } else {
            vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10);

            deal(address(adapter), specifiedAmount);
        }

        uint256 eth_balance = address(adapter).balance;
        uint256 eEth_balance = eEth_.balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, eth_, address(eEth_), side, specifiedAmount);

        if (trade.calculatedAmount > 0) {
            if (side == OrderSide.Buy) {
                assertGe(
                    specifiedAmount,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
                /// @dev Transfer function contains rounding errors because of
                /// rewards in eETH contract, therefore we assume a +/-2
                /// tolerance
                assertLe(
                    specifiedAmount - 2,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
                assertEq(
                    trade.calculatedAmount,
                    eth_balance - address(adapter).balance
                );
            } else {
                assertEq(
                    specifiedAmount, eth_balance - address(adapter).balance
                );
                assertEq(
                    trade.calculatedAmount,
                    eEth_.balanceOf(address(this)) - eEth_balance
                );
            }
        }
    }

    function testSwapFuzzEtherfiEthWeEth(uint256 specifiedAmount, bool isBuy)
        public
    {
        OrderSide side = isBuy ? OrderSide.Buy : OrderSide.Sell;

        address eth_ = address(0);
        IERC20 weEth_ = IERC20(address(weEth));
        bytes32 pair = bytes32(0);
        uint256[] memory limits = adapter.getLimits(pair, eth_, address(weEth_));

        if (side == OrderSide.Buy) {
            vm.assume(specifiedAmount < limits[1] && specifiedAmount > 10);

            deal(address(adapter), weEth_.totalSupply());
        } else {
            vm.assume(specifiedAmount < limits[0] && specifiedAmount > 10);

            deal(address(adapter), specifiedAmount);
        }

        uint256 eth_balance = address(adapter).balance;
        uint256 weEth_balance = weEth_.balanceOf(address(this));

        Trade memory trade =
            adapter.swap(pair, eth_, address(weEth_), side, specifiedAmount);

        if (trade.calculatedAmount > 0) {
            if (side == OrderSide.Buy) {
                assertGe(
                    specifiedAmount,
                    weEth_.balanceOf(address(this)) - weEth_balance
                );
                /// @dev Transfer function contains rounding errors because of
                /// rewards in eETH contract, therefore we assume a +/-4
                /// tolerance
                assertLe(
                    specifiedAmount - 4,
                    weEth_.balanceOf(address(this)) - weEth_balance
                );
                assertEq(
                    trade.calculatedAmount,
                    eth_balance - address(adapter).balance
                );
            } else {
                assertEq(
                    specifiedAmount, eth_balance - address(adapter).balance
                );
                assertEq(
                    trade.calculatedAmount,
                    weEth_.balanceOf(address(this)) - weEth_balance
                );
            }
        }
    }

    function testSwapSellIncreasingEtherfi() public {
        executeIncreasingSwapsEtherfi(OrderSide.Sell);
    }

    function testSwapBuyIncreasingEtherfi() public {
        executeIncreasingSwapsEtherfi(OrderSide.Buy);
    }

    function executeIncreasingSwapsEtherfi(OrderSide side) internal {
        bytes32 pair = bytes32(0);

        uint256 amountConstant_ = 10 ** 18;

        uint256[] memory amounts = new uint256[](TEST_ITERATIONS);
        amounts[0] = amountConstant_;
        for (uint256 i = 1; i < TEST_ITERATIONS; i++) {
            amounts[i] = amountConstant_ * i;
        }

        Trade[] memory trades = new Trade[](TEST_ITERATIONS);
        uint256 beforeSwap;
        for (uint256 i = 1; i < TEST_ITERATIONS; i++) {
            beforeSwap = vm.snapshot();

            deal(address(weEth), address(this), amounts[i]);
            IERC20(address(weEth)).approve(address(adapter), amounts[i]);

            trades[i] = adapter.swap(
                pair,
                address(address(weEth)),
                address(address(eEth)),
                side,
                amounts[i]
            );
            vm.revertTo(beforeSwap);
        }

        for (uint256 i = 1; i < TEST_ITERATIONS - 1; i++) {
            assertLe(trades[i].calculatedAmount, trades[i + 1].calculatedAmount);
            assertLe(trades[i].gasUsed, trades[i + 1].gasUsed);
        }
    }

    function testGetCapabilitiesEtherfi(bytes32 pair, address t0, address t1)
        public
        view
    {
        Capability[] memory res =
            adapter.getCapabilities(pair, address(t0), address(t1));

        assertEq(res.length, 3);
    }

    function testGetTokensEtherfi() public view {
        bytes32 pair = bytes32(0);
        address[] memory tokens = adapter.getTokens(pair);

        assertEq(tokens.length, 3);
    }

    function testGetLimitsEtherfi() public view {
        bytes32 pair = bytes32(0);
        uint256[] memory limits =
            adapter.getLimits(pair, address(eEth), address(weEth));

        assertEq(limits.length, 2);
    }
}
