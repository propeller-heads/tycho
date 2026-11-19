pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "@src/executors/CurveExecutor.sol";
import {
    ALICE,
    Constants,
    DAI_ADDR,
    ETH_ADDR,
    STETH_ADDR,
    USDC_ADDR,
    USDE_ADDR,
    USDT_ADDR,
    WBTC_ADDR,
    WETH_ADDR
} from "../Constants.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";
import {ExecutorDeployment} from "../ExecutorDeployment.sol";
import {
    CurveDeployment,
    STETH_POOL,
    TRICRYPTO_POOL,
    TRIPOOL
} from "../deployments/Curve.sol";

address constant SUSD_ADDR =
    address(0x57Ab1ec28D129707052df4dF418D58a2D46d5f51);
address constant FRAX_ADDR =
    address(0x853d955aCEf822Db058eb8505911ED77F175b99e);
address constant DOLA_ADDR =
    address(0x865377367054516e17014CcdED1e7d814EDC9ce4);
address constant XYO_ADDR = address(0x55296f69f40Ea6d20E478533C15A6B08B654E758);
address constant UWU_ADDR = address(0x55C08ca52497e2f1534B59E2917BF524D4765257);
address constant CRVUSD_ADDR =
    address(0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E);
address constant WSTTAO_ADDR =
    address(0xe9633C52f4c8B7BDeb08c4A7fE8a5c1B84AFCf67);
address constant WTAO_ADDR =
    address(0x77E06c9eCCf2E797fd462A92B6D7642EF85b0A44);
address constant BSGG_ADDR =
    address(0xdA16Cf041E2780618c49Dbae5d734B89a6Bac9b3);
address constant FRAXPYUSD_POOL =
    address(0xA5588F7cdf560811710A2D82D3C9c99769DB1Dcb);
address constant TRICRYPTO2_POOL = 0xD51a44d3FaE010294C616388b506AcdA1bfAAE46;
address constant SUSD_POOL = 0xA5407eAE9Ba41422680e2e00537571bcC53efBfD;
address constant FRAX_USDC_POOL = 0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2;
address constant USDE_USDC_POOL = 0x02950460E2b9529D0E00284A5fA2d7bDF3fA4d72;
address constant DOLA_FRAXPYUSD_POOL =
    0xef484de8C07B6e2d732A92B5F78e81B38f99f95E;
address constant ETH_XYO_POOL = 0x99e09ee2d6Bb16c0F5ADDfEA649dbB2C1d524624;
address constant UWU_WETH_POOL = 0x77146B0a1d08B6844376dF6d9da99bA7F1b19e71;
address constant CRVUSD_USDT_POOL = 0x390f3595bCa2Df7d23783dFd126427CCeb997BF4;
address constant WSTTAO_WTAO_POOL = 0xf2DCf6336D8250754B4527f57b275b19c8D5CF88;
address constant BSGG_USDT_POOL = 0x5500307Bcf134E5851FB4D7D8D1Dc556dCdB84B4;
// Curve meta registry
address constant CURVE_META_REGISTRY =
    0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC;

interface ICurvePool {
    function coins(uint256 i) external view returns (address);
}

// Curve pool registry
// This is the registry that contains the information about the pool
// The naming convention is different because it is in vyper
interface MetaRegistry {
    function get_n_coins(address pool) external view returns (uint256);

    function get_coin_indices(address pool, address from, address to)
        external
        view
        returns (int128, int128, bool);
}

contract CurveExecutorExposed is CurveExecutor {
    constructor(address _nativeToken, address _stEthAddress)
        CurveExecutor(_nativeToken, _stEthAddress)
    {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            address tokenOut,
            address pool,
            uint8 poolType,
            uint8 i,
            uint8 j
        )
    {
        return _decodeData(data);
    }
}

contract CurveExecutorTest is Test, TestUtils, Constants {
    using SafeERC20 for IERC20;

    CurveExecutorExposed curveExecutorExposed;
    MetaRegistry metaRegistry;

    function setUp() public {
        uint256 forkBlock = 22031795;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        curveExecutorExposed = new CurveExecutorExposed(ETH_ADDR, STETH_ADDR);
        metaRegistry = MetaRegistry(CURVE_META_REGISTRY);
    }

    function testDecodeParams() public view {
        bytes memory data = abi.encodePacked(
            WETH_ADDR, USDC_ADDR, TRICRYPTO_POOL, uint8(3), uint8(2), uint8(0)
        );

        (
            address tokenIn,
            address tokenOut,
            address pool,
            uint8 poolType,
            uint8 i,
            uint8 j
        ) = curveExecutorExposed.decodeData(data);

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertEq(pool, TRICRYPTO_POOL);
        assertEq(poolType, 3);
        assertEq(i, 2);
        assertEq(j, 0);
    }

    function testGetTransferData() public {
        bytes memory data = abi.encodePacked(
            WETH_ADDR, USDC_ADDR, TRICRYPTO_POOL, uint8(3), uint8(2), uint8(0)
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = curveExecutorExposed.getTransferData(data);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, TRICRYPTO_POOL);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertEq(outputToRouter, true);
    }

    function testRevertTokenInAddressZero() public {
        bytes memory data = abi.encodePacked(
            address(0), USDC_ADDR, TRICRYPTO_POOL, uint8(3), uint8(2), uint8(0)
        );

        vm.expectRevert(CurveExecutor__TokenAddressZero.selector);
        curveExecutorExposed.swap(1 ether, data, ALICE);
    }

    function testRevertTokenOutAddressZero() public {
        bytes memory data = abi.encodePacked(
            WETH_ADDR, address(0), TRICRYPTO_POOL, uint8(3), uint8(2), uint8(0)
        );

        vm.expectRevert(CurveExecutor__TokenAddressZero.selector);
        curveExecutorExposed.swap(1 ether, data, ALICE);
    }

    function testTriPool() public {
        // Swapping DAI -> USDC on TriPool 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7
        uint256 amountIn = 1 ether;
        deal(DAI_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(DAI_ADDR, USDC_ADDR, TRIPOOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(DAI_ADDR).approve(TRIPOOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(curveExecutorExposed)), 999797
        );
    }

    function testStEthPool() public {
        // Swapping ETH -> stETH on StEthPool 0xDC24316b9AE028F1497c275EB9192a3Ea0f67022
        uint256 amountIn = 1 ether;
        deal(address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(ETH_ADDR, STETH_ADDR, STETH_POOL, 1);

        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(STETH_ADDR).balanceOf(address(curveExecutorExposed)),
            1001072414418410897
        );
    }

    function testTricrypto2Pool() public {
        // Swapping WETH -> WBTC on Tricrypto2Pool 0xD51a44d3FaE010294C616388b506AcdA1bfAAE46
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(WETH_ADDR, WBTC_ADDR, TRICRYPTO2_POOL, 3);

        vm.prank(address(curveExecutorExposed));
        IERC20(WETH_ADDR).approve(TRICRYPTO2_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(WBTC_ADDR).balanceOf(address(curveExecutorExposed)), 2279618
        );
    }

    function testSUSDPool() public {
        // Swapping USDC -> SUSD on SUSDPool 0xA5407eAE9Ba41422680e2e00537571bcC53efBfD
        uint256 amountIn = 100 * 10 ** 6;
        deal(USDC_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(USDC_ADDR, SUSD_ADDR, SUSD_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(USDC_ADDR).approve(SUSD_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(SUSD_ADDR).balanceOf(address(curveExecutorExposed)),
            100488101605550214590
        );
    }

    function testFraxUsdcPool() public {
        // Swapping FRAX -> USDC on FraxUsdcPool 0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2
        uint256 amountIn = 1 ether;
        deal(FRAX_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(FRAX_ADDR, USDC_ADDR, FRAX_USDC_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(FRAX_ADDR).approve(FRAX_USDC_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(curveExecutorExposed)), 998097
        );
    }

    function testUsdeUsdcPool() public {
        // Swapping USDC -> USDE on a CryptoSwapNG, deployed by factory 0x6A8cbed756804B16E05E741eDaBd5cB544AE21bf (plain pool)
        uint256 amountIn = 100 * 10 ** 6;
        deal(USDC_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(USDC_ADDR, USDE_ADDR, USDE_USDC_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(USDC_ADDR).approve(USDE_USDC_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(USDE_ADDR).balanceOf(address(curveExecutorExposed)),
            100064812138999986170
        );
    }

    function testDolaFraxPyusdPool() public {
        // Swapping DOLA -> FRAXPYUSD on a CryptoSwapNG, deployed by factory 0x6A8cbed756804B16E05E741eDaBd5cB544AE21bf (meta pool)
        uint256 amountIn = 100 * 10 ** 6;
        deal(DOLA_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data =
            _getData(DOLA_ADDR, FRAXPYUSD_POOL, DOLA_FRAXPYUSD_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(DOLA_ADDR).approve(DOLA_FRAXPYUSD_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(FRAXPYUSD_POOL).balanceOf(address(curveExecutorExposed)),
            99688992
        );
    }

    function testCryptoPoolWithETH() public {
        // Swapping XYO -> ETH on a CryptoPool, deployed by factory 0xF18056Bbd320E96A48e3Fbf8bC061322531aac99
        uint256 amountIn = 1 ether;
        deal(XYO_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(XYO_ADDR, ETH_ADDR, ETH_XYO_POOL, 2);

        vm.prank(address(curveExecutorExposed));
        IERC20(XYO_ADDR).approve(ETH_XYO_POOL, amountIn);

        uint256 ethBefore = address(curveExecutorExposed).balance;
        curveExecutorExposed.swap(amountIn, data, ALICE);
        uint256 ethAfter = address(curveExecutorExposed).balance;

        assertEq(ethAfter - ethBefore, 6081816039338);
    }

    function testCryptoPool() public {
        // Swapping BSGG -> USDT on a CryptoPool, deployed by factory 0xF18056Bbd320E96A48e3Fbf8bC061322531aac99
        uint256 amountIn = 1000 ether;
        deal(BSGG_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(BSGG_ADDR, USDT_ADDR, BSGG_USDT_POOL, 2);

        vm.prank(address(curveExecutorExposed));
        IERC20(BSGG_ADDR).approve(BSGG_USDT_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(USDT_ADDR).balanceOf(address(curveExecutorExposed)), 23429
        );
    }

    function testTricryptoPool() public {
        // Swapping WETH -> USDC on a Tricrypto pool, deployed by factory 0x0c0e5f2fF0ff18a3be9b835635039256dC4B4963
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(WETH_ADDR, USDC_ADDR, TRICRYPTO_POOL, 2);

        vm.prank(address(curveExecutorExposed));
        IERC20(WETH_ADDR).approve(TRICRYPTO_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(curveExecutorExposed)),
            1861130974
        );
    }

    function testTwoCryptoPool() public {
        // Swapping UWU -> WETH on a Twocrypto pool, deployed by factory 0x98ee851a00abee0d95d08cf4ca2bdce32aeaaf7f
        uint256 amountIn = 1 ether;
        deal(UWU_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data = _getData(UWU_ADDR, WETH_ADDR, UWU_WETH_POOL, 2);

        vm.prank(address(curveExecutorExposed));
        IERC20(UWU_ADDR).approve(UWU_WETH_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(curveExecutorExposed)),
            2873786684675
        );
    }

    function testStableSwapPool() public {
        // Swapping CRVUSD -> USDT on a StableSwap pool, deployed by factory 0x4F8846Ae9380B90d2E71D5e3D042dff3E7ebb40d (plain pool)
        uint256 amountIn = 1 ether;
        deal(USDT_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data =
            _getData(USDT_ADDR, CRVUSD_ADDR, CRVUSD_USDT_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(USDT_ADDR).forceApprove(CRVUSD_USDT_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(CRVUSD_ADDR).balanceOf(address(curveExecutorExposed)),
            10436946786333182306400100
        );
    }

    function testMetaPool() public {
        // Swapping WTAO -> WSTTAO on a MetaPool deployed by factory 0xB9fC157394Af804a3578134A6585C0dc9cc990d4 (plain pool)
        uint256 amountIn = 100 * 10 ** 9; // 9 decimals
        deal(WTAO_ADDR, address(curveExecutorExposed), amountIn);

        bytes memory data =
            _getData(WTAO_ADDR, WSTTAO_ADDR, WSTTAO_WTAO_POOL, 1);

        vm.prank(address(curveExecutorExposed));
        IERC20(WTAO_ADDR).approve(WSTTAO_WTAO_POOL, amountIn);
        curveExecutorExposed.swap(amountIn, data, ALICE);

        assertEq(
            IERC20(WSTTAO_ADDR).balanceOf(address(curveExecutorExposed)),
            32797923610
        );
    }

    function _getData(
        address tokenIn,
        address tokenOut,
        address pool,
        uint8 poolType
    ) internal view returns (bytes memory data) {
        (int128 i, int128 j) = _getIndexes(tokenIn, tokenOut, pool);
        data = abi.encodePacked(
            tokenIn,
            tokenOut,
            pool,
            poolType,
            uint8(uint256(uint128(i))),
            uint8(uint256(uint128(j)))
        );
    }

    function _getIndexes(address tokenIn, address tokenOut, address pool)
        internal
        view
        returns (int128, int128)
    {
        (int128 coinInIndex, int128 coinOutIndex,) =
            metaRegistry.get_coin_indices(pool, tokenIn, tokenOut);
        return (coinInIndex, coinOutIndex);
    }
}

contract TychoRouterForCurveTest is TychoRouterTestSetup, CurveDeployment {
    function testSingleCurveIntegration() public {
        deal(UWU_ADDR, ALICE, 1 ether);

        vm.startPrank(ALICE);
        IERC20(UWU_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_curve");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 2877855391767);

        vm.stopPrank();
    }

    function testStEthPoolWithInitialStEth() public {
        // Swapping ETH -> stETH on the Curve stETH pool through
        // the full TychoRouterV3 workflow. Uses native ETH as input
        uint256 amountIn = 1 ether;
        deal(ALICE, amountIn);

        vm.startPrank(ALICE);

        bytes memory curveStEthData = abi.encodePacked(
            ETH_ADDR,
            STETH_ADDR,
            STETH_POOL,
            uint8(1), // poolType = stable
            uint8(0), // i = 0 (ETH)
            uint8(1) // j = 1 (stETH)
        );

        bytes memory swap = encodeSingleSwap(curveExecutor, curveStEthData);

        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn,
            ETH_ADDR, // tokenIn = native ETH
            STETH_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE,
            noClientFee(),
            swap
        );

        // pools reports sending 999958043830457008 stETH to msg.sender (router)
        // router actually got 999958043830457007 stETH
        // after the last transfer to Alice, she gets 999958043830457005 stETH
        assertEq(amountOut, 999958043830457005);
        assertEq(IERC20(STETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(ALICE.balance, 0);

        vm.stopPrank();
    }
}
