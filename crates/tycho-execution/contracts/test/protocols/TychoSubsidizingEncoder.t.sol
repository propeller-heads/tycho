// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {SubsidyPayer} from "@src/SubsidyPayer.sol";

/// @dev Runs subsidized swaps encoded and signed by
///      tests/subsidy_integration_tests.rs. Each `<name>_unsigned` entry is
///      the same calldata with the encoder's zero signature, which fills
///      without the subsidy.
contract TychoSubsidizingEncoderTest is TychoRouterTestSetup {
    // Must match the constants in tests/subsidy_integration_tests.rs.
    address constant SUBSIDIZING_EXECUTOR =
        0xE06C68f4c151619a8F32d475e91349B3D820343a;
    address payable constant SUBSIDY_PAYER =
        payable(0xB45394E79c96A1eF3DF90ebc93B31E87aeB1AAdA);
    uint256 constant SUBSIDY_SIGNER_PK =
        0xd05309bfe92ede9638264d4537fba0c605f619ff6923f5118f44c035b708163a;
    uint256 constant SUBSIDY_NONCE = 42;
    uint256 constant DAI_SUBSIDY = 5 ether;
    uint256 constant USDC_SUBSIDY = 5_000000;
    uint256 constant WETH_SUBSIDY = 0.002 ether;
    uint256 constant ETH_SUBSIDY = 0.002 ether;

    SubsidyPayer payer;

    function setUp() public override {
        super.setUp();
        address[] memory pausers = new address[](0);
        deployCodeTo(
            "SubsidyPayer.sol:SubsidyPayer",
            abi.encode(
                tychoRouterAddr,
                vm.addr(SUBSIDY_SIGNER_PK),
                SubsidyPayer.Roles({
                    signerSetter: makeAddr("signerSetter"),
                    budgetManager: makeAddr("budgetManager"),
                    pauserAdmin: makeAddr("pauserAdmin"),
                    unpauser: makeAddr("unpauser"),
                    pausers: pausers
                })
            ),
            SUBSIDY_PAYER
        );
        payer = SubsidyPayer(SUBSIDY_PAYER);
        deployCodeTo(
            "TychoSubsidizingExecutor.sol:TychoSubsidizingExecutor",
            abi.encode(SUBSIDY_PAYER),
            SUBSIDIZING_EXECUTOR
        );

        address[] memory executors = new address[](1);
        executors[0] = SUBSIDIZING_EXECUTOR;
        vm.warp(block.timestamp - tychoRouter.DELAY_EXECUTOR_ACTIVATION());
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);

        deal(DAI_ADDR, SUBSIDY_PAYER, 100 ether);
        deal(USDC_ADDR, SUBSIDY_PAYER, 100_000000);
        deal(WETH_ADDR, SUBSIDY_PAYER, 1 ether);
        vm.deal(SUBSIDY_PAYER, 1 ether);
    }

    /// @dev Runs the unsigned and the signed calldata from the same state and
    ///      returns how much `balanceOf` rose for each.
    function _run(
        string memory testName,
        address tokenIn,
        uint256 amountIn,
        function() view returns (uint256) balanceOf
    ) internal returns (uint256 unsignedAmountOut, uint256 signedAmountOut) {
        uint256 snapshot = vm.snapshotState();
        unsignedAmountOut = _call(
            string.concat(testName, "_unsigned"), tokenIn, amountIn, balanceOf
        );
        assertFalse(payer.isNonceUsed(SUBSIDY_NONCE));
        vm.revertToState(snapshot);

        signedAmountOut = _call(testName, tokenIn, amountIn, balanceOf);
        assertTrue(payer.isNonceUsed(SUBSIDY_NONCE));
    }

    function _call(
        string memory testName,
        address tokenIn,
        uint256 amountIn,
        function() view returns (uint256) balanceOf
    ) internal returns (uint256 amountOut) {
        deal(tokenIn, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        IERC20(tokenIn).approve(PERMIT2_ADDRESS, amountIn);
        uint256 balanceBefore = balanceOf();
        (bool success,) = tychoRouterAddr.call(loadCallDataFromFile(testName));
        vm.stopPrank();
        assertTrue(success, "Call Failed");
        amountOut = balanceOf() - balanceBefore;
    }

    function _aliceDai() internal view returns (uint256) {
        return IERC20(DAI_ADDR).balanceOf(ALICE);
    }

    function _aliceUsdc() internal view returns (uint256) {
        return IERC20(USDC_ADDR).balanceOf(ALICE);
    }

    function _aliceWeth() internal view returns (uint256) {
        return IERC20(WETH_ADDR).balanceOf(ALICE);
    }

    function _aliceEth() internal view returns (uint256) {
        return ALICE.balance;
    }

    function _aliceVaultDai() internal view returns (uint256) {
        return tychoRouter.balanceOf(ALICE, uint256(uint160(DAI_ADDR)));
    }

    function testSubsidizedUniswapV2SingleSwap() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_uniswap_v2_single_swap",
            WETH_ADDR,
            1 ether,
            _aliceDai
        );

        assertEq(unsignedOut, 2018817438608734439722);
        assertEq(signedOut, unsignedOut + DAI_SUBSIDY);
    }

    function testSubsidizedSequentialSwapEndingInUniswapV3() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_sequential_swap_ending_in_uniswap_v3",
            DAI_ADDR,
            3000 ether,
            _aliceUsdc
        );

        assertEq(unsignedOut, 2983381989);
        assertEq(signedOut, unsignedOut + USDC_SUBSIDY);
    }

    function testSubsidizedSplitSwapLargerShare() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_split_swap_larger_share",
            WETH_ADDR,
            1 ether,
            _aliceDai
        );

        assertEq(unsignedOut, 2019190678597940916235);
        assertEq(signedOut, unsignedOut + DAI_SUBSIDY);
    }

    function testSubsidizedSplitSwapAfterIntermediate() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_split_swap_after_intermediate",
            WETH_ADDR,
            1 ether,
            _aliceUsdc
        );

        assertEq(unsignedOut, 1989737355);
        assertEq(signedOut, unsignedOut + USDC_SUBSIDY);
    }

    function testSubsidizedUniswapV4EthPayout() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_uniswap_v4_eth_payout",
            USDC_ADDR,
            3000_000000,
            _aliceEth
        );

        assertEq(unsignedOut, 1474406268748155809);
        assertEq(signedOut, unsignedOut + ETH_SUBSIDY);
    }

    function testSubsidizedPoolSwapBeforeUnwrap() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_pool_swap_before_unwrap",
            DAI_ADDR,
            3000 ether,
            _aliceEth
        );

        assertEq(unsignedOut, 1475644707225677606);
        assertEq(signedOut, unsignedOut + WETH_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSubsidizedVaultCreditSwap() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_vault_credit_swap",
            WETH_ADDR,
            1 ether,
            _aliceVaultDai
        );

        assertEq(unsignedOut, 2018817438608734439722);
        assertEq(signedOut, unsignedOut + DAI_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), signedOut);
    }

    function testSubsidizedPoolSwapBeforeWrap() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_pool_swap_before_wrap",
            USDC_ADDR,
            3000_000000,
            _aliceWeth
        );

        assertEq(unsignedOut, 1474406268748155809);
        assertEq(signedOut, unsignedOut + ETH_SUBSIDY);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSubsidizedSwapWithClientFeeAndPermit2() public {
        (uint256 unsignedOut, uint256 signedOut) = _run(
            "test_subsidized_swap_with_client_fee_and_permit2",
            WETH_ADDR,
            1 ether,
            _aliceDai
        );

        assertEq(unsignedOut, 1998629264222647095325);
        // The 1% client fee also applies to the subsidy.
        assertEq(signedOut, unsignedOut + DAI_SUBSIDY * 99 / 100);
    }
}
