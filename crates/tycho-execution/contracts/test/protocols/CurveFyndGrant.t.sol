// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {CurveFyndPolicy} from "./CurveFyndPolicy.sol";
import {
    CurveExecutor__InvalidDataLength
} from "@src/executors/CurveExecutor.sol";
import {ICurveCryptoPool, ICurveFyndPolicy} from "@interfaces/ICurvePool.sol";

interface ITwocryptoFactory {
    function admin() external view returns (address);

    // forge-lint: disable-next-line(mixed-case-function) Vyper function name.
    function deploy_pool(
        string calldata name,
        string calldata symbol,
        address[2] calldata coins,
        uint256 implementationId,
        uint256 a,
        uint256 gamma,
        uint256 midFee,
        uint256 outFee,
        uint256 feeGamma,
        uint256 allowedExtraProfit,
        uint256 adjustmentStep,
        uint256 maExpTime,
        uint256 initialPrice
    ) external returns (address);
}

interface ITwocryptoInitPool {
    function initialize(
        uint256 reservedProfitFraction,
        uint256 adminFee,
        address policy,
        uint256 initialPrice,
        address[] calldata allowlistAdd
    ) external;

    // forge-lint: disable-next-line(mixed-case-function) Vyper function name.
    function add_liquidity(uint256[2] calldata amounts, uint256 minMintAmount)
        external
        returns (uint256);
}

/// @notice Deploys a USDC/WETH pool with the parameters of Curve's Fynd pilot
/// script and attaches the test port of its policy.
abstract contract CurveFyndPilotPool is Test {
    // Twocrypto-ng factory and its "fx50" implementation, active since block
    // 25131091.
    address constant TWOCRYPTO_FACTORY =
        0x98EE851a00abeE0d95D08cF4CA2BdCE32aeaAF7F;
    uint256 constant FORK_BLOCK = 26408000;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    // The Rust integration test signs with this key for this policy address.
    uint256 constant CONTROLLER_KEY =
        0x1b6f2b1f4a8f0c1e5d3a7b9c2e4f6a8b0c1d3e5f7a9b2c4d6e8f0a1b3c5d7e9f;
    address constant POLICY_ADDRESS =
        0xf1Fd000000000000000000000000000000C0FFeE;
    uint256 constant DEFAULT_FEE = 100_000_000; // 1%
    uint256 constant MIN_FEE = 100_000; // 0.1 bp

    address pool;
    CurveFyndPolicy policy;

    function _deployPilotPool(address router) internal {
        address admin = ITwocryptoFactory(TWOCRYPTO_FACTORY).admin();
        uint256 initialPrice = 3000e18; // USDC per WETH
        pool = ITwocryptoFactory(TWOCRYPTO_FACTORY)
            .deploy_pool(
                "fynd-pilot wethusdc",
                "fynd-wethusdc",
                [USDC, WETH],
                uint256(keccak256("fx50")),
                50_000,
                11_111_111_111,
                100_000_000,
                100_000_000,
                1e16,
                0,
                5e15,
                865,
                initialPrice
            );
        deployCodeTo(
            "CurveFyndPolicy.sol:CurveFyndPolicy",
            abi.encode(
                pool, vm.addr(CONTROLLER_KEY), router, DEFAULT_FEE, MIN_FEE
            ),
            POLICY_ADDRESS
        );
        policy = CurveFyndPolicy(POLICY_ADDRESS);
        vm.prank(admin);
        ITwocryptoInitPool(pool)
            .initialize(
                2_500_000_000,
                5_000_000_000,
                POLICY_ADDRESS,
                initialPrice,
                new address[](0)
            );

        address lp = makeAddr("lp");
        deal(USDC, lp, 3_000_000e6);
        deal(WETH, lp, 1000 ether);
        vm.startPrank(lp);
        IERC20(USDC).approve(pool, type(uint256).max);
        IERC20(WETH).approve(pool, type(uint256).max);
        ITwocryptoInitPool(pool)
            .add_liquidity([uint256(3_000_000e6), 1000 ether], 0);
        vm.stopPrank();
    }

    function _signGrant(ICurveFyndPolicy.Grant memory grant)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                policy.domainSeparator(),
                keccak256(abi.encode(policy.GRANT_TYPEHASH(), grant))
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(CONTROLLER_KEY, digest);
        return abi.encodePacked(r, s, v);
    }

    /// @dev USDC (coin 0) to WETH (coin 1) through the pilot pool, with a
    /// grant for `taker` when `fee` is non-zero.
    function _swapData(
        address taker,
        uint256 amountIn,
        uint256 fee,
        uint64 nonce
    ) internal view returns (bytes memory data) {
        data = abi.encodePacked(USDC, WETH, pool, uint8(2), uint8(0), uint8(1));
        if (fee == 0) return data;
        ICurveFyndPolicy.Grant memory grant = ICurveFyndPolicy.Grant({
            taker: taker,
            coin_in: 0,
            min_amount_in: amountIn,
            max_amount_in: amountIn,
            fee: fee,
            nonce: nonce,
            deadline: uint32(block.timestamp + 60)
        });
        data = abi.encodePacked(
            data,
            grant.min_amount_in,
            grant.max_amount_in,
            grant.fee,
            grant.nonce,
            grant.deadline,
            _signGrant(grant)
        );
    }
}

contract CurveExecutorFyndGrantTest is CurveFyndPilotPool, Constants {
    CurveExecutor executor;
    uint256 constant AMOUNT_IN = 3000e6;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        executor = new CurveExecutor(ETH_ADDR, STETH_ADDR);
        // Called directly, the executor is the policy's router.
        _deployPilotPool(address(executor));
    }

    function _swap(bytes memory data) internal returns (uint256) {
        deal(USDC, address(executor), AMOUNT_IN);
        vm.prank(address(executor));
        IERC20(USDC).approve(pool, AMOUNT_IN);
        uint256 balanceBefore = IERC20(WETH).balanceOf(address(executor));
        executor.swap(AMOUNT_IN, data, ALICE);
        return IERC20(WETH).balanceOf(address(executor)) - balanceBefore;
    }

    function testSwapWithoutGrantPaysDefaultFee() public {
        uint256 quoted = ICurveCryptoPool(pool).get_dy(0, 1, AMOUNT_IN);

        uint256 amountOut = _swap(_swapData(address(this), AMOUNT_IN, 0, 0));

        assertEq(amountOut, quoted);
    }

    function testSwapWithGrantPaysGrantedFee() public {
        uint256 quotedAtDefaultFee =
            ICurveCryptoPool(pool).get_dy(0, 1, AMOUNT_IN);

        uint256 amountOut =
            _swap(_swapData(address(this), AMOUNT_IN, MIN_FEE, 7));

        // The quote before its 1% fee, less the 0.1 bp grant fee.
        uint256 beforeFee = quotedAtDefaultFee * 1e10 / (1e10 - DEFAULT_FEE);
        assertApproxEqRel(amountOut, beforeFee * (1e10 - MIN_FEE) / 1e10, 1e12);
        assertTrue(policy.is_nonce_used(7));
    }

    function testRevertGrantForOtherTaker() public {
        bytes memory data = _swapData(BOB, AMOUNT_IN, MIN_FEE, 7);

        deal(USDC, address(executor), AMOUNT_IN);
        vm.prank(address(executor));
        IERC20(USDC).approve(pool, AMOUNT_IN);
        vm.expectRevert(bytes("signature"));
        executor.swap(AMOUNT_IN, data, ALICE);
    }

    function testRevertReusedNonce() public {
        bytes memory data = _swapData(address(this), AMOUNT_IN, MIN_FEE, 7);
        _swap(data);

        deal(USDC, address(executor), AMOUNT_IN);
        vm.prank(address(executor));
        IERC20(USDC).approve(pool, AMOUNT_IN);
        vm.expectRevert(bytes("nonce"));
        executor.swap(AMOUNT_IN, data, ALICE);
    }

    function testRevertGrantOutsideInputRange() public {
        bytes memory data = _swapData(address(this), AMOUNT_IN - 1, MIN_FEE, 7);

        deal(USDC, address(executor), AMOUNT_IN);
        vm.prank(address(executor));
        IERC20(USDC).approve(pool, AMOUNT_IN);
        vm.expectRevert(bytes("amount"));
        executor.swap(AMOUNT_IN, data, ALICE);
    }

    function testRevertTruncatedGrant() public {
        bytes memory data = _swapData(address(this), AMOUNT_IN, MIN_FEE, 7);
        assembly {
            mstore(data, sub(mload(data), 1))
        }

        vm.expectRevert(CurveExecutor__InvalidDataLength.selector);
        executor.swap(AMOUNT_IN, data, ALICE);
    }
}

contract TychoRouterForCurveFyndGrantTest is
    TychoRouterTestSetup,
    CurveFyndPilotPool
{
    function getForkBlock() public pure override returns (uint256) {
        return FORK_BLOCK;
    }

    function setUp() public override {
        super.setUp();
        _deployPilotPool(tychoRouterAddr);
    }

    function testSingleCurveFyndGrantIntegration() public {
        uint256 amountIn = 3000e6;
        deal(USDC, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC).approve(tychoRouterAddr, amountIn);
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_curve_fynd_grant"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertTrue(policy.is_nonce_used(1));
        // At the 0.1 bp grant fee. The 1% default fee pays about 0.99 WETH.
        assertEq(IERC20(WETH).balanceOf(ALICE), 999823362657189392);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
    }
}
