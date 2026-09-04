pragma solidity ^0.8.26;

import {IUniswapV3Pool} from "../src/executors/UniswapV3Executor.sol";
import {CREATE3} from "@solady/utils/CREATE3.sol";

// Test utilities and mocks
import "./Constants.sol";
import "./TestUtils.sol";
import {Permit2TestHelper} from "./Permit2TestHelper.sol";
import {ClientFeeTestHelper} from "./ClientFeeTestHelper.sol";

// Core contracts
import "@src/TychoRouterV3.sol";
import "@src/FeeCalculator.sol";

contract TychoRouterExposed is TychoRouterV3 {
    constructor(
        address permit2_,
        address feeCalculator,
        address pauser,
        address unpauser,
        address executorSetter,
        address routerFeeSetter
    )
        TychoRouterV3(
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
    address[] private _deployedExecutors;

    TychoRouterExposed tychoRouter;
    address tychoRouterAddr;

    // Executors are deployed and registered by `deployExecutors`; one is exposed here
    // only when a test outside this file uses it.
    address public balancerv2Executor;
    address public curveExecutor;
    address public fluidV1Executor;
    address public nativeWrapExecutor;
    address public propAMMFallbackExecutor;
    address public ringSwapV2Executor;
    address public rocketpoolExecutor;
    address public slipstreamsExecutor;
    address public usv2Executor;
    address public usv3Executor;
    address public usv4Executor;

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

    /// Deploys `<name>Executor` with CREATE3, at an address derived from the contract name and
    /// this test contract alone: a CREATE2 proxy keyed by `keccak256("<name>Executor")` performs
    /// the CREATE, so the address does not depend on the order in which the executors are
    /// deployed, on the executor's own bytecode, or on where its source sits in the tree. The
    /// addresses pinned in `config/test_executor_addresses.json` — and embedded in the calldata
    /// fixtures the Rust tests generate from it — therefore only need to change when an executor
    /// is renamed. Constructors run normally; `msg.sender` inside them is the proxy and
    /// `address(this)` is the final executor address.
    function _deployExecutor(string memory name, bytes memory constructorArgs)
        internal
        returns (address executor)
    {
        string memory contractName = string.concat(name, "Executor");
        executor = CREATE3.deployDeterministic(
            abi.encodePacked(vm.getCode(contractName), constructorArgs),
            keccak256(bytes(contractName))
        );
        _deployedExecutors.push(executor);
    }

    /// Deploys `<name>Executor`, which takes no constructor arguments.
    function _deployExecutor(string memory name)
        internal
        returns (address executor)
    {
        return _deployExecutor(name, "");
    }

    /// Deploys every executor and returns them in deployment order, for registration on the
    /// router. Executors are listed by source path; a new one goes wherever its path sorts.
    function deployExecutors() public returns (address[] memory) {
        address ekuboCore = 0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444;
        address ekuboMevResist = 0x553a2EFc570c9e104942cEC6aC1c18118e54C091;
        address poolManager = 0x000000000004444c5dc75cB358380D2e3dE08A90;

        delete _deployedExecutors;
        _deployExecutor("AerodromeV1");
        balancerv2Executor = _deployExecutor("BalancerV2");
        _deployExecutor("BalancerV3");
        _deployExecutor("Bebop", abi.encode(BEBOP_SETTLEMENT, BEBOP_ROUTER));
        _deployExecutor("BopAMM", abi.encode(BOPAMM_SETTLEMENT));
        curveExecutor =
            _deployExecutor("Curve", abi.encode(ETH_ADDR, STETH_ADDR));
        _deployExecutor("Ekubo", abi.encode(ekuboCore, ekuboMevResist));
        _deployExecutor("EkuboV3");
        _deployExecutor("ERC4626");

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
        _deployExecutor(
            "Etherfi",
            abi.encode(
                ETH_ADDR,
                EETH_ADDR,
                LIQUIDITY_POOL_ADDR,
                WEETH_ADDR,
                REDEMPTION_MANAGER_ADDR
            )
        );
        _deployExecutor("FermiSwap", abi.encode(FERMI_SWAPPER));
        fluidV1Executor =
            _deployExecutor("FluidV1", abi.encode(FLUIDV1_LIQUIDITY));
        _deployExecutor("Hashflow", abi.encode(HASHFLOW_ROUTER));
        _deployExecutor("LiquidityParty");

        // Etch placeholder bytecode if Liquorice contracts are not yet
        // deployed at this fork block.
        if (LIQUORICE_SETTLEMENT.code.length == 0) {
            vm.etch(LIQUORICE_SETTLEMENT, bytes("1"));
        }
        if (LIQUORICE_BALANCE_MANAGER.code.length == 0) {
            vm.etch(LIQUORICE_BALANCE_MANAGER, bytes("1"));
        }
        _deployExecutor(
            "Liquorice",
            abi.encode(LIQUORICE_SETTLEMENT, LIQUORICE_BALANCE_MANAGER)
        );
        _deployExecutor("MaverickV2");
        _deployExecutor("Metric", abi.encode(METRIC_ORACLE));
        nativeWrapExecutor =
            _deployExecutor("NativeWrap", abi.encode(WETH_ADDR));
        _deployExecutor("PropAMM");
        propAMMFallbackExecutor = _deployExecutor("PropAMMFallback");
        ringSwapV2Executor = _deployExecutor(
            "RingSwapV2", abi.encode(RING_FEW_FACTORY, RING_SWAP_FACTORY)
        );
        rocketpoolExecutor =
            _deployExecutor("Rocketpool", abi.encode(ROCKET_DEPOSIT_POOL));

        // The Sky venues exist only on mainnet, and the executor's constructor
        // reads their token wiring, so it cannot deploy on forks where the
        // venues have no code.
        if (SKY_DAI_USDS_CONVERTER.code.length != 0) {
            _deployExecutor(
                "Sky",
                abi.encode(
                    SKY_LITE_PSM, SKY_USDS_PSM_WRAPPER, SKY_DAI_USDS_CONVERTER
                )
            );
        }
        slipstreamsExecutor = _deployExecutor("Slipstreams");
        usv2Executor = _deployExecutor("UniswapV2", abi.encode(uint256(30)));
        usv3Executor = _deployExecutor("UniswapV3");
        usv4Executor = _deployExecutor(
            "UniswapV4", abi.encode(poolManager, ANGSTROM_HOOK)
        );

        return _deployedExecutors;
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
