pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/ekubo_v3/EkuboV3Executor.sol";
import "@src/executors/ekubo_v3/EkuboV3RobinhoodExecutor.sol";
import {
    IFlashAccountant,
    ILocker
} from "@ekubo-v3/interfaces/IFlashAccountant.sol";
import {Constants} from "../Constants.sol";
import {SafeTransferLib} from "@solady/utils/SafeTransferLib.sol";
import {LibCall} from "@solady/utils/LibCall.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {createConcentratedPoolConfig} from "@ekubo-v3/types/poolConfig.sol";
import {createPositionId, PositionId} from "@ekubo-v3/types/positionId.sol";
import {
    SignedSwapMeta,
    createSignedSwapMeta
} from "@ekubo-v3/types/signedSwapMeta.sol";
import {ControllerAddress} from "@ekubo-v3/types/controllerAddress.sol";
import {
    ISignedExclusiveSwap
} from "@ekubo-v3/interfaces/extensions/ISignedExclusiveSwap.sol";
import {
    SignedExclusiveSwapLib
} from "@ekubo-v3/libraries/SignedExclusiveSwapLib.sol";
// Imported so Forge compiles the artifact for deployCodeTo.
import {
    SignedExclusiveSwap
} from "@ekubo-v3/extensions/SignedExclusiveSwap.sol";

// Handles callbacks directly and receives the native token directly
contract EkuboV3ExecutorStandalone is EkuboV3Executor, ILocker {
    function locked_6416899205(
        uint256 /* id */
    )
        external
    {
        // swapData layout in msg.data: selector(4) | id(32) | amountIn(16) | receiver(20) | tokenIn(20) | ...
        // tokenIn starts at byte 72 (4 + 32 + 16 + 20 = 72)
        // Swap data already uses ETH_ADDRESS for native ETH;
        // getTransferData returns it as-is.
        address tokenIn = address(bytes20(msg.data[72:92]));

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (tokenIn == ETH_ADDRESS) {
            assert(
                transferType
                    == TransferManager.TransferType.TransferNativeInExecutor
            );
        } else {
            assert(transferType == TransferManager.TransferType.Transfer);
        }
        uint256 amount = uint128(bytes16(msg.data[36:52]));

        if (tokenIn != ETH_ADDRESS) {
            IERC20(tokenIn).transfer(receiver, amount);
        }
        bytes memory res = handleCallback(msg.data);
        assembly ("memory-safe") {
            return(add(res, 32), mload(res))
        }
    }

    // To receive withdrawals from Core
    receive() external payable {}
}

contract EkuboV3RobinhoodExecutorStandalone is
    EkuboV3RobinhoodExecutor,
    EkuboV3ExecutorStandalone
{
    // Diamond-inheritance disambiguation only; super resolves to
    // EkuboV3RobinhoodExecutor.
    function _swapHop(
        PoolKey memory poolKey,
        SwapParameters swapParameters,
        bytes calldata swapData,
        uint256 offset
    )
        internal
        override(EkuboV3Executor, EkuboV3RobinhoodExecutor)
        returns (PoolBalanceUpdate, uint256)
    {
        return super._swapHop(poolKey, swapParameters, swapData, offset);
    }
}

contract EkuboV3ExecutorTest is Constants, TestUtils {
    using SignedExclusiveSwapLib for ISignedExclusiveSwap;

    EkuboV3ExecutorStandalone immutable executor =
        new EkuboV3ExecutorStandalone();

    LiquidityHelper immutable liquidityHelper = new LiquidityHelper();

    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 USDT = IERC20(USDT_ADDR);

    // Controller PK whose vm.addr() has high bit clear (first
    // nibble 0-7) so SignedExclusiveSwap treats it as an EOA.
    uint256 constant CONTROLLER_PK = 0xBEEF;
    address constant SIGNED_SWAP_ADMIN = address(0xAD);

    bytes32 constant ORACLE_CONFIG =
        0x517E506700271AEa091b02f42756F5E174Af5230000000000000000000000000;

    constructor() {
        vm.makePersistent(address(executor));
        vm.makePersistent(address(liquidityHelper));
    }

    modifier setUpFork(uint256 blockNumber) {
        vm.createSelectFork(vm.rpcUrl("mainnet"), blockNumber);
        // TODO: remove once Foundry stable includes the Fusaka hardfork mapping.
        // Forks use the chain's hardfork, not foundry.toml's evm_version.
        // The current stable (Dec 2024) predates Fusaka (Dec 2025),
        // so Osaka opcodes aren't enabled for post-Fusaka blocks.
        // See: https://github.com/foundry-rs/foundry/issues/13040
        address(vm)
            .call(abi.encodeWithSignature("setEvmVersion(string)", "osaka"));

        _;
    }

    function testSingleSwapEth() public setUpFork(24218590) {
        uint256 amountIn = 1 ether;

        deal(address(executor), amountIn);

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        bytes memory data = abi.encodePacked(
            ETH_ADDRESS, // tokenIn
            USDC_ADDR, // tokenOut
            ORACLE_CONFIG // poolConfig
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut =
            USDC.balanceOf(address(executor)) - usdcBalanceBeforeExecutor;

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore + amountIn);
        assertEq(address(executor).balance, ethBalanceBeforeExecutor - amountIn);

        assertEq(
            USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore - amountOut
        );
    }

    function testSingleSwapERC20() public setUpFork(24218590) {
        uint256 amountIn = 1_000_000_000;

        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        bytes memory data = abi.encodePacked(
            USDC_ADDR, // tokenIn
            ETH_ADDRESS, // tokenOut
            ORACLE_CONFIG // config
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut = address(executor).balance - ethBalanceBeforeExecutor;

        assertEq(USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore + amountIn);
        assertEq(
            USDC.balanceOf(address(executor)),
            usdcBalanceBeforeExecutor - amountIn
        );

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore - amountOut);
    }

    function testMevCapture() public setUpFork(24198199) {
        uint256 amountIn = 1_000;

        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        uint256 usdtBalanceBeforeCore = USDT.balanceOf(CORE_ADDRESS);
        uint256 usdtBalanceBeforeExecutor = USDT.balanceOf(address(executor));

        bytes memory data = abi.encodePacked(
            USDC_ADDR, // tokenIn
            USDT_ADDR, // tokenOut
            bytes32(
                0x5555ff9ff2757500bf4ee020dcfd0210cffa41be000053e2d6238da480000032
            ) // config (0.0005% fee and 0.005% tick spacing, mev capture ext)
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut =
            USDT.balanceOf(address(executor)) - usdtBalanceBeforeExecutor;

        assertEq(USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore + amountIn);
        assertEq(
            USDC.balanceOf(address(executor)),
            usdcBalanceBeforeExecutor - amountIn
        );

        assertEq(
            USDT.balanceOf(CORE_ADDRESS), usdtBalanceBeforeCore - amountOut
        );
    }

    // Data is generated by test case in swap_encoder::tests::ekubo_v3::test_encode_swap_multi
    function testMultiHopSwapIntegration() public setUpFork(24218590) {
        uint256 amountIn = 1 ether;
        deal(address(executor), amountIn);

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        uint256 usdtBalanceBeforeCore = USDT.balanceOf(CORE_ADDRESS);
        uint256 usdtBalanceBeforeAlice = USDT.balanceOf(ALICE);

        executor.swap(
            amountIn,
            loadCallDataFromFile("test_ekubo_v3_encode_swap_multi"),
            ALICE
        );

        uint256 amountOut = USDT.balanceOf(ALICE) - usdtBalanceBeforeAlice;

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore + amountIn);
        assertEq(address(executor).balance, ethBalanceBeforeExecutor - amountIn);

        assertEq(
            USDT.balanceOf(CORE_ADDRESS), usdtBalanceBeforeCore - amountOut
        );
    }

    function testSignedExclusiveSwap() public setUpFork(24218590) {
        // --- 1. Deploy SignedExclusiveSwap extension ---
        deployCodeTo(
            "SignedExclusiveSwap.sol",
            abi.encode(CORE, SIGNED_SWAP_ADMIN),
            SIGNED_EXCLUSIVE_SWAP_ADDRESS
        );
        ISignedExclusiveSwap ext =
            ISignedExclusiveSwap(SIGNED_EXCLUSIVE_SWAP_ADDRESS);

        // --- 2. Initialize a zero-fee pool ---
        // USDC < USDT, so token0 = USDC, token1 = USDT.
        PoolConfig poolConfig = createConcentratedPoolConfig({
            _fee: 0,
            _tickSpacing: 100,
            _extension: SIGNED_EXCLUSIVE_SWAP_ADDRESS
        });
        PoolKey memory poolKey =
            PoolKey({token0: USDC_ADDR, token1: USDT_ADDR, config: poolConfig});

        address controller = vm.addr(CONTROLLER_PK);
        vm.prank(SIGNED_SWAP_ADMIN);
        ext.initializePool(
            poolKey,
            0, // tick = 0 → ~1:1 price
            ControllerAddress.wrap(controller)
        );

        // --- 3. Add liquidity ---
        uint256 amount0 = 10_000_000_000; // 10k USDC
        uint256 amount1 = 10_000_000_000; // 10k USDT
        deal(USDC_ADDR, address(liquidityHelper), amount0);
        deal(USDT_ADDR, address(liquidityHelper), amount1);
        liquidityHelper.provide(
            poolKey,
            -1000, // tickLower
            1000, // tickUpper
            1_000_000_000 // liquidity
        );

        // --- 4. Prepare signed swap ---
        uint256 amountIn = 100_000; // 0.1 USDC
        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBefore = USDC.balanceOf(address(executor));
        uint256 usdtBefore = USDT.balanceOf(address(executor));

        // Build SignedSwapMeta: no authorized locker, 1h
        // deadline, zero fee, nonce 0
        SignedSwapMeta meta = createSignedSwapMeta({
            _authorizedLocker: address(0),
            _deadline: uint32(block.timestamp + 1 hours),
            _fee: 0,
            _nonce: 0
        });

        // Minimum balance update: accept any output
        PoolBalanceUpdate minBU = PoolBalanceUpdate.wrap(
            bytes32(
                0x8000000000000000000000000000000080000000000000000000000000000000
            )
        );

        // Sign with controller key
        bytes32 digest =
            ext.hashSignedSwapPayload(poolKey.toPoolId(), meta, minBU);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(CONTROLLER_PK, digest);
        bytes memory signature = abi.encodePacked(r, s, v);

        // --- 5. Build hop data and execute ---
        // Wire: tokenIn(20) | tokenOut(20) | poolConfig(32) |
        //   meta(32) | minBU(32) | sigLen(2) | sig(N)
        bytes memory data = abi.encodePacked(
            USDC_ADDR,
            USDT_ADDR,
            poolConfig,
            bytes32(SignedSwapMeta.unwrap(meta)),
            PoolBalanceUpdate.unwrap(minBU),
            uint16(signature.length),
            signature
        );

        executor.swap(amountIn, data, address(executor));

        // --- 6. Assert ---
        uint256 usdcSpent = usdcBefore - USDC.balanceOf(address(executor));
        uint256 usdtReceived = USDT.balanceOf(address(executor)) - usdtBefore;

        assertEq(usdcSpent, amountIn);
        assertEq(usdtReceived, 99990);
    }

    // Data is generated by test case in
    // swap_encoder::tests::ekubo_v3::test_encode_signed_swap_integration
    function testSignedSwapIntegration() public setUpFork(24218590) {
        // Warp to 1 hour before the deadline (1_752_000_000)
        // embedded in the Rust-encoded calldata.
        vm.warp(1_752_000_000 - 1 hours);

        // --- 1. Deploy SignedExclusiveSwap extension ---
        deployCodeTo(
            "SignedExclusiveSwap.sol",
            abi.encode(CORE, SIGNED_SWAP_ADMIN),
            SIGNED_EXCLUSIVE_SWAP_ADDRESS
        );
        ISignedExclusiveSwap ext =
            ISignedExclusiveSwap(SIGNED_EXCLUSIVE_SWAP_ADDRESS);

        // --- 2. Initialize a zero-fee USDC/USDT pool ---
        PoolConfig poolConfig = createConcentratedPoolConfig({
            _fee: 0,
            _tickSpacing: 100,
            _extension: SIGNED_EXCLUSIVE_SWAP_ADDRESS
        });
        PoolKey memory poolKey =
            PoolKey({token0: USDC_ADDR, token1: USDT_ADDR, config: poolConfig});

        address controller = vm.addr(CONTROLLER_PK);
        vm.prank(SIGNED_SWAP_ADMIN);
        ext.initializePool(
            poolKey,
            0, // tick = 0 → ~1:1 price
            ControllerAddress.wrap(controller)
        );

        // --- 3. Add liquidity ---
        deal(USDC_ADDR, address(liquidityHelper), 10_000_000_000);
        deal(USDT_ADDR, address(liquidityHelper), 10_000_000_000);
        liquidityHelper.provide(
            poolKey,
            -1000, // tickLower
            1000, // tickUpper
            1_000_000_000 // liquidity
        );

        // --- 4. Execute swap from Rust-encoded calldata ---
        uint256 amountIn = 100_000; // 0.1 USDC
        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBefore = USDC.balanceOf(address(executor));
        uint256 usdtBefore = USDT.balanceOf(address(executor));

        executor.swap(
            amountIn,
            loadCallDataFromFile("test_ekubo_v3_signed_swap_integration"),
            address(executor)
        );

        // --- 5. Assert ---
        uint256 usdcSpent = usdcBefore - USDC.balanceOf(address(executor));
        uint256 usdtReceived = USDT.balanceOf(address(executor)) - usdtBefore;

        assertEq(usdcSpent, amountIn);
        assertEq(usdtReceived, 99990);
    }
}

/// Adds liquidity to an Ekubo V3 pool via Core flash accounting.
/// Tokens must be held by this contract before calling `provide`.
contract LiquidityHelper {
    function provide(
        PoolKey memory poolKey,
        int32 tickLower,
        int32 tickUpper,
        int128 liquidity
    ) external {
        // Start payment tracking before lock.
        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.startPayments.selector, poolKey.token0
            )
        );
        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.startPayments.selector, poolKey.token1
            )
        );

        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodePacked(
                IFlashAccountant.lock.selector,
                abi.encode(poolKey, tickLower, tickUpper, liquidity)
            )
        );
    }

    function locked_6416899205(
        uint256 /* id */
    )
        external
    {
        (
            PoolKey memory poolKey,
            int32 tickLower,
            int32 tickUpper,
            int128 liquidity
        ) = abi.decode(msg.data[36:], (PoolKey, int32, int32, int128));

        PositionId posId = createPositionId(bytes24(0), tickLower, tickUpper);
        PoolBalanceUpdate bu = CORE.updatePosition(poolKey, posId, liquidity);

        // Transfer exactly the required amounts to Core.
        if (bu.delta0() > 0) {
            SafeTransferLib.safeTransfer(
                poolKey.token0, CORE_ADDRESS, uint128(bu.delta0())
            );
        }
        if (bu.delta1() > 0) {
            SafeTransferLib.safeTransfer(
                poolKey.token1, CORE_ADDRESS, uint128(bu.delta1())
            );
        }

        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.completePayments.selector, poolKey.token0
            )
        );
        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.completePayments.selector, poolKey.token1
            )
        );
    }
}

contract EkuboV3RobinhoodExecutorTest is Constants, TestUtils {
    EkuboV3RobinhoodExecutorStandalone immutable executor =
        new EkuboV3RobinhoodExecutorStandalone();

    // The token pair of the deployed WETH/USDG Ve33 pool on Robinhood Chain.
    address constant RHC_WETH = 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
    address constant RHC_USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;

    constructor() {
        vm.makePersistent(address(executor));
    }

    modifier setUpFork() {
        vm.createSelectFork(vm.rpcUrl("robinhood"));
        // Precautionary, mirroring the mainnet fork workaround; Foundry
        // defaults unknown chains to the latest hardfork.
        address(vm)
            .call(abi.encodeWithSignature("setEvmVersion(string)", "osaka"));
        _;
    }

    // Runs on a Robinhood Chain fork against the deployed Ve33 extension,
    // using the token pair of a live Ve33 pool. A fresh pool (distinct tick
    // spacing) is initialized because the executor needs a funded position
    // at a known price, not a specific market.
    function testVe33Swap() public setUpFork {
        // Ve33 pools require zero fee and power-of-four tick spacing.
        PoolConfig poolConfig = createConcentratedPoolConfig({
            _fee: 0, _tickSpacing: 64, _extension: VE33_ADDRESS
        });
        PoolKey memory poolKey =
            PoolKey({token0: RHC_WETH, token1: RHC_USDG, config: poolConfig});
        CORE.initializePool(poolKey, 0);

        LiquidityHelper liquidityHelper = new LiquidityHelper();
        deal(RHC_WETH, address(liquidityHelper), 10_000_000_000);
        deal(RHC_USDG, address(liquidityHelper), 10_000_000_000);
        liquidityHelper.provide(poolKey, -960, 960, 1_000_000_000);

        uint256 amountIn = 100_000;
        deal(RHC_WETH, address(executor), amountIn);
        uint256 usdgBalanceBefore =
            IERC20(RHC_USDG).balanceOf(address(executor));

        executor.swap(
            amountIn,
            abi.encodePacked(RHC_WETH, RHC_USDG, PoolConfig.unwrap(poolConfig)),
            address(executor)
        );

        assertGt(
            IERC20(RHC_USDG).balanceOf(address(executor)), usdgBalanceBefore
        );
    }
}

contract TychoRouterForEkuboV3Test is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24218590;
    }

    function setUp() public virtual override {
        super.setUp();

        // TODO: remove once Foundry stable includes the Fusaka hardfork mapping.
        address(vm)
            .call(abi.encodeWithSignature("setEvmVersion(string)", "osaka"));

        // Remove delegations
        vm.etch(ALICE, "");
    }

    function testSingleEkuboV3Integration() public {
        deal(ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        (bool success,) = tychoRouterAddr.call{value: 1 ether}(
            loadCallDataFromFile("test_single_encoding_strategy_ekubo_v3")
        );

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 26173932);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testTwoEkuboV3Integration() public {
        // Test multi-hop Ekubo V3 swaps (grouped swap)
        //
        // USDT ──(EKUBO V3)──> USDC ──(EKUBO V3)──> ETH
        //
        deal(USDT_ADDR, ALICE, 10_000_000_000);
        uint256 balanceBefore = ALICE.balance;

        vm.startPrank(ALICE);
        SafeTransferLib.safeApprove(
            USDT_ADDR, tychoRouterAddr, type(uint256).max
        );

        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_single_ekubo_v3_grouped_swap")
        );
        assertTrue(success, "call failed");

        assertEq(ALICE.balance - balanceBefore, 2500939754680596105);
        assertEq(IERC20(USDT_ADDR).balanceOf(ALICE), 0);
    }
}
