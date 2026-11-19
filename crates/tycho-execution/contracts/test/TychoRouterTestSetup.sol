pragma solidity ^0.8.26;

import {
    IUniswapV3Pool
} from "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import {CREATE3} from "@solady/utils/CREATE3.sol";
import {
    ExecutorDeployment,
    ExecutorDeployments
} from "./ExecutorDeployment.sol";

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
        address[] memory executors,
        address[] memory pausers,
        address unpauser,
        address executorSetter,
        address routerFeeSetter
    )
        TychoRouterV3(
            permit2_,
            feeCalculator,
            executors,
            pausers,
            unpauser,
            executorSetter,
            routerFeeSetter
        )
    {}

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

    /// Authorises the swap to `transferFrom` `amountIn` of `tokenIn` from the
    /// caller, then runs the split swap.
    function exposedSplitSwapWithTransferFrom(
        address tokenIn,
        uint256 amountIn,
        uint256 nTokens,
        bytes calldata swaps,
        address receiver,
        bool isCyclical
    ) external returns (uint256) {
        _tstoreTransferFromInfo(tokenIn, amountIn, false, false);
        return _splitSwap(amountIn, nTokens, swaps, receiver, isCyclical);
    }

    /// Sequential-swap counterpart of `exposedSplitSwapWithTransferFrom`.
    function exposedSequentialSwapWithTransferFrom(
        address tokenIn,
        uint256 amountIn,
        bytes calldata swaps,
        address receiver
    ) external returns (uint256) {
        _tstoreTransferFromInfo(tokenIn, amountIn, false, false);
        return _sequentialSwap(amountIn, swaps, receiver);
    }

    function exposedDeltaAccounting(address token, uint256 amount) external {
        _updateDeltaAccounting(token, int256(amount));
    }

    function exposedGetFeeCalculator() external view returns (address) {
        return this.getFeeCalculator();
    }
}

abstract contract TychoRouterTestSetup is
    Constants,
    Permit2TestHelper,
    ClientFeeTestHelper,
    TestUtils,
    ExecutorDeployments
{
    TychoRouterExposed tychoRouter;
    address tychoRouterAddr;

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

        forkTimestamp = vm.getBlockTimestamp();
        uint256 setupTime = forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR;
        vm.warp(setupTime);

        vm.startPrank(ADMIN);
        tychoRouter = deployRouter();
        deployDummyContract();
        vm.stopPrank();
        // The router deploys with the vault paused; most tests use the vault.
        vm.prank(UNPAUSER);
        tychoRouter.unpauseVault();

        address[] memory executors = deployExecutors();
        vm.startPrank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.stopPrank();

        // The fee calculator is only deployed here because if we do it before the router and executors ALL the addresses will change and this will break a lot of tests
        deployFeeCalculator();
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(feeCalculator));
        // Warp past the timelock and activate
        vm.warp(
            vm.getBlockTimestamp()
                + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION()
        );
        vm.prank(FEE_SETTER);
        tychoRouter.activateFeeCalculator();
        vm.warp(forkTimestamp);
    }

    function listTestPausers() internal view returns (address[] memory) {
        address[] memory pausers = new address[](1);
        pausers[0] = PAUSER;
        return pausers;
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
            new address[](0),
            listTestPausers(),
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
        tychoRouterAddr = address(tychoRouter);
        return tychoRouter;
    }

    /// `msg.sender` in the executor constructors is the CREATE3 proxy.
    function deployExecutors() private returns (address[] memory deployed) {
        deployed = new address[](deploymentHooks.length);
        for (uint256 i = 0; i < deploymentHooks.length; i++) {
            ExecutorDeployment memory d = deploymentHooks[i]();
            deployed[i] = CREATE3.deployDeterministic(
                abi.encodePacked(vm.getCode(d.contractName), d.constructorArgs),
                keccak256(bytes(d.protocolSystem))
            );
        }
    }

    function deployFeeCalculator() public {
        // Deploy and configure FeeCalculator
        routerFeeReceiver = makeAddr("routerFeeReceiver");
        // clientFeeReceiver is the address corresponding to CLIENT_FEE_RECEIVER_PK
        clientFeeReceiver = vm.addr(CLIENT_FEE_RECEIVER_PK);
        feeCalculator = new FeeCalculator(FEE_SETTER, routerFeeReceiver);
        // The calculator captures the full surplus from its constructor.
        // The swap tests quote a round `expectedAmountOut` below the real pool
        // output and assert the receiver gets that whole output, so the capture
        // bps is set to zero here and the capture tests set it back.
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterPositiveSlippageBps(0);
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
