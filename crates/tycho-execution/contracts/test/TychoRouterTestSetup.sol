pragma solidity ^0.8.26;

// Executors
import {BalancerV2Executor} from "../src/executors/BalancerV2Executor.sol";
import {BalancerV3Executor} from "../src/executors/BalancerV3Executor.sol";
import {BebopExecutor} from "../src/executors/BebopExecutor.sol";
import {CurveExecutor} from "../src/executors/CurveExecutor.sol";
import {EkuboExecutor} from "../src/executors/EkuboExecutor.sol";
import {EkuboV3Executor} from "../src/executors/EkuboV3Executor.sol";
import {EtherfiExecutor} from "../src/executors/EtherfiExecutor.sol";
import {
    LiquidityPartyExecutor
} from "../src/executors/LiquidityPartyExecutor.sol";
import {HashflowExecutor} from "../src/executors/HashflowExecutor.sol";
import {MaverickV2Executor} from "../src/executors/MaverickV2Executor.sol";
import {UniswapV2Executor} from "../src/executors/UniswapV2Executor.sol";
import {
    UniswapV3Executor,
    IUniswapV3Pool
} from "../src/executors/UniswapV3Executor.sol";
import {UniswapV4Executor} from "../src/executors/UniswapV4Executor.sol";
import {FluidV1Executor} from "../src/executors/FluidV1Executor.sol";
import {SlipstreamsExecutor} from "../src/executors/SlipstreamsExecutor.sol";
import {RocketpoolExecutor} from "../src/executors/RocketpoolExecutor.sol";
import {ERC4626Executor} from "../src/executors/ERC4626Executor.sol";
import {NativeWrapExecutor} from "../src/executors/NativeWrapExecutor.sol";
import {LiquoriceExecutor} from "../src/executors/LiquoriceExecutor.sol";
import {AerodromeV1Executor} from "../src/executors/AerodromeV1Executor.sol";
// Test utilities and mocks
import "./Constants.sol";
import "./TestUtils.sol";
import {Permit2TestHelper} from "./Permit2TestHelper.sol";
import {ClientFeeTestHelper} from "./ClientFeeTestHelper.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";

// Core contracts
import "@src/TychoRouter.sol";
import "@src/FeeCalculator.sol";

contract TychoRouterExposed is TychoRouter {
    constructor(
        address permit2_,
        address feeCalculator,
        address pauser,
        address unpauser,
        address executorSetter,
        address routerFeeSetter
    )
        TychoRouter(
            permit2_,
            feeCalculator,
            pauser,
            unpauser,
            executorSetter,
            routerFeeSetter
        )
    {}

    function tstoreExposed(
        address tokenIn,
        uint256 amountIn,
        bool isPermit2,
        bool useVault
    ) external {
        _tstoreTransferFromInfo(tokenIn, amountIn, isPermit2, useVault);
    }

    function exposedSplitSwap(
        uint256 amountIn,
        uint256 nTokens,
        bytes calldata swaps,
        address receiver,
        bool isCyclical
    ) external returns (uint256) {
        return _splitSwap(amountIn, nTokens, swaps, receiver, isCyclical);
    }

    function exposedSequentialSwap(
        uint256 amountIn,
        bytes calldata swaps,
        address receiver
    ) external returns (uint256) {
        return _sequentialSwap(amountIn, swaps, receiver);
    }

    function exposedDeltaAccounting(address token, uint256 amount) external {
        _updateDeltaAccounting(token, int256(amount));
    }

    function exposedGetFeeCalculator() external view returns (address) {
        return this.getFeeCalculator();
    }
}

contract TychoRouterTestSetup is
    Constants,
    Permit2TestHelper,
    ClientFeeTestHelper,
    TestUtils
{
    TychoRouterExposed tychoRouter;
    address tychoRouterAddr;
    UniswapV2Executor public usv2Executor;
    UniswapV3Executor public usv3Executor;
    UniswapV3Executor public pancakev3Executor;
    UniswapV4Executor public usv4Executor;
    BalancerV2Executor public balancerv2Executor;
    EkuboExecutor public ekuboExecutor;
    CurveExecutor public curveExecutor;
    MaverickV2Executor public maverickv2Executor;
    BalancerV3Executor public balancerV3Executor;
    BebopExecutor public bebopExecutor;
    HashflowExecutor public hashflowExecutor;
    FluidV1Executor public fluidV1Executor;
    SlipstreamsExecutor public slipstreamsExecutor;
    RocketpoolExecutor public rocketpoolExecutor;
    ERC4626Executor public erc4626Executor;
    NativeWrapExecutor public nativeWrapExecutor;
    EkuboV3Executor public ekuboV3Executor;
    EtherfiExecutor public etherfiExecutor;
    LiquidityPartyExecutor public liquidityPartyExecutor;
    LiquoriceExecutor public liquoriceExecutor;
    AerodromeV1Executor public aerodromeV1Executor;

    FeeCalculator feeCalculator;
    address routerFeeReceiver;
    address clientFeeReceiver;

    function getChain() public view virtual returns (string memory) {
        return "mainnet";
    }

    function getForkBlock() public view virtual returns (uint256) {
        return 22082754;
    }

    uint256 internal forkTimestamp;

    function setUp() public virtual {
        string memory chain = getChain();
        uint256 forkBlock = getForkBlock();
        vm.createSelectFork(vm.rpcUrl(chain), forkBlock);

        forkTimestamp = block.timestamp;
        uint256 setupTime = forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR;
        vm.warp(setupTime);

        vm.startPrank(ADMIN);
        tychoRouter = deployRouter();
        deployDummyContract();
        vm.stopPrank();

        address[] memory executors = deployExecutors();
        vm.startPrank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.stopPrank();

        // The fee calculator is only deployed here because if we do it before the router and executors ALL the addresses will change and this will break a lot of tests
        deployFeeCalculator();
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(feeCalculator));
        // Warp past the timelock and activate
        vm.warp(block.timestamp + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION());
        vm.prank(FEE_SETTER);
        tychoRouter.activateFeeCalculator();
        vm.warp(forkTimestamp);
    }

    function deployRouter() public returns (TychoRouterExposed) {
        // Use vm.etch to place dummy bytecode at address(123) so it passes the
        // .code.length check in the constructor without deploying a contract
        // (which would shift all subsequent addresses and break pre-generated permit2 signatures)
        address placeholderFeeCalculator = address(123);
        vm.etch(placeholderFeeCalculator, hex"00");

        tychoRouter = new TychoRouterExposed(
            PERMIT2_ADDRESS,
            placeholderFeeCalculator,
            PAUSER,
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
        tychoRouterAddr = address(tychoRouter);
        return tychoRouter;
    }

    function deployExecutors() public returns (address[] memory) {
        address poolManagerAddress = 0x000000000004444c5dc75cB358380D2e3dE08A90;
        address ekuboCore = 0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444;
        address ekuboMevResist = 0x553a2EFc570c9e104942cEC6aC1c18118e54C091;

        IPoolManager poolManager = IPoolManager(poolManagerAddress);
        usv2Executor = new UniswapV2Executor(30);
        usv3Executor = new UniswapV3Executor();
        usv4Executor = new UniswapV4Executor(poolManager, ANGSTROM_HOOK);
        pancakev3Executor = new UniswapV3Executor();
        balancerv2Executor = new BalancerV2Executor();
        ekuboExecutor = new EkuboExecutor(ekuboCore, ekuboMevResist);
        curveExecutor = new CurveExecutor(ETH_ADDR, STETH_ADDR);
        maverickv2Executor = new MaverickV2Executor();
        balancerV3Executor = new BalancerV3Executor();
        bebopExecutor = new BebopExecutor(BEBOP_SETTLEMENT);
        hashflowExecutor = new HashflowExecutor(HASHFLOW_ROUTER);
        fluidV1Executor = new FluidV1Executor(FLUIDV1_LIQUIDITY);
        slipstreamsExecutor = new SlipstreamsExecutor();
        rocketpoolExecutor = new RocketpoolExecutor(ROCKET_DEPOSIT_POOL);
        erc4626Executor = new ERC4626Executor();
        nativeWrapExecutor = new NativeWrapExecutor(WETH_ADDR);
        ekuboV3Executor = new EkuboV3Executor();
        // Etch placeholder bytecode if Etherfi contracts are not yet deployed
        // on this chain/block (e.g. non-mainnet forks or early mainnet blocks).
        if (EETH_ADDR.code.length == 0) vm.etch(EETH_ADDR, bytes("1"));
        if (LIQUIDITY_POOL_ADDR.code.length == 0) {
            vm.etch(LIQUIDITY_POOL_ADDR, bytes("1"));
        }
        if (WEETH_ADDR.code.length == 0) vm.etch(WEETH_ADDR, bytes("1"));
        if (REDEMPTION_MANAGER_ADDR.code.length == 0) {
            vm.etch(REDEMPTION_MANAGER_ADDR, bytes("1"));
        }
        etherfiExecutor = new EtherfiExecutor(
            ETH_ADDR,
            EETH_ADDR,
            LIQUIDITY_POOL_ADDR,
            WEETH_ADDR,
            REDEMPTION_MANAGER_ADDR
        );
        // Etch placeholder bytecode if Liquorice contracts are not yet
        // deployed at this fork block.
        if (LIQUORICE_SETTLEMENT.code.length == 0) {
            vm.etch(LIQUORICE_SETTLEMENT, bytes("1"));
        }
        if (LIQUORICE_BALANCE_MANAGER.code.length == 0) {
            vm.etch(LIQUORICE_BALANCE_MANAGER, bytes("1"));
        }
        liquoriceExecutor = new LiquoriceExecutor(
            LIQUORICE_SETTLEMENT, LIQUORICE_BALANCE_MANAGER
        );
        liquidityPartyExecutor = new LiquidityPartyExecutor();
        aerodromeV1Executor = new AerodromeV1Executor();

        address[] memory executors = new address[](21);
        executors[0] = address(usv2Executor);
        executors[1] = address(usv3Executor);
        executors[2] = address(pancakev3Executor);
        executors[3] = address(usv4Executor);
        executors[4] = address(balancerv2Executor);
        executors[5] = address(ekuboExecutor);
        executors[6] = address(curveExecutor);
        executors[7] = address(maverickv2Executor);
        executors[8] = address(balancerV3Executor);
        executors[9] = address(bebopExecutor);
        executors[10] = address(hashflowExecutor);
        executors[11] = address(fluidV1Executor);
        executors[12] = address(slipstreamsExecutor);
        executors[13] = address(rocketpoolExecutor);
        executors[14] = address(erc4626Executor);
        executors[15] = address(nativeWrapExecutor);
        executors[16] = address(ekuboV3Executor);
        executors[17] = address(etherfiExecutor);
        executors[18] = address(liquoriceExecutor);
        executors[19] = address(liquidityPartyExecutor);
        executors[20] = address(aerodromeV1Executor);
        return executors;
    }

    function deployFeeCalculator() public {
        // Deploy and configure FeeCalculator
        routerFeeReceiver = makeAddr("routerFeeReceiver");
        // clientFeeReceiver is the address corresponding to CLIENT_FEE_RECEIVER_PK
        clientFeeReceiver = vm.addr(CLIENT_FEE_RECEIVER_PK);
        feeCalculator = new FeeCalculator(FEE_SETTER);
    }

    function pleEncode(bytes[] memory data)
        public
        pure
        returns (bytes memory encoded)
    {
        for (uint256 i = 0; i < data.length; i++) {
            encoded = bytes.concat(
                encoded,
                abi.encodePacked(bytes2(uint16(data[i].length)), data[i])
            );
        }
    }

    function encodeSingleSwap(address executor, bytes memory protocolData)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(executor, protocolData);
    }

    function encodeSequentialSwap(address executor, bytes memory protocolData)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(executor, protocolData);
    }

    function encodeSplitSwap(
        uint8 tokenInIndex,
        uint8 tokenOutIndex,
        uint24 split,
        address executor,
        bytes memory protocolData
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(
            tokenInIndex, tokenOutIndex, split, executor, protocolData
        );
    }

    function encodeUniswapV2Swap(
        address target,
        address tokenIn,
        address tokenOut
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(target, tokenIn, tokenOut);
    }

    function encodeUniswapV3Swap(
        address tokenIn,
        address tokenOut,
        address target,
        bool zero2one
    ) internal view returns (bytes memory) {
        IUniswapV3Pool pool = IUniswapV3Pool(target);
        return abi.encodePacked(tokenIn, tokenOut, pool.fee(), target, zero2one);
    }
}
