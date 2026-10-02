// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {stdJson} from "forge-std/StdJson.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {
    IUnlockCallback
} from "@uniswap/v4-core/src/interfaces/callback/IUnlockCallback.sol";
import {
    Currency,
    CurrencyLibrary
} from "@uniswap/v4-core/src/types/Currency.sol";
import {
    BalanceDelta,
    BalanceDeltaLibrary
} from "@uniswap/v4-core/src/types/BalanceDelta.sol";
import {PoolKey} from "@uniswap/v4-core/src/types/PoolKey.sol";
import {
    ModifyLiquidityParams
} from "@uniswap/v4-core/src/types/PoolOperation.sol";
import {IHooks} from "@uniswap/v4-core/src/interfaces/IHooks.sol";

import {TychoRouterV3, ClientFeeParams} from "../src/TychoRouterV3.sol";
import {UniswapV4Executor} from "../src/executors/UniswapV4Executor.sol";
import {UniswapXFiller} from "../src/uniswap_x/UniswapXFiller.sol";
import {
    InputToken,
    OrderInfo,
    OutputToken,
    ResolvedOrder,
    SignedOrder
} from "../src/uniswap_x/IStructs.sol";

contract Aqua0TychoForkToken is ERC20 {
    constructor(string memory name, string memory symbol) ERC20(name, symbol) {}

    function mint(address receiver, uint256 amount) external {
        _mint(receiver, amount);
    }
}

contract Aqua0TychoLiquiditySeeder is IUnlockCallback {
    using BalanceDeltaLibrary for BalanceDelta;
    using CurrencyLibrary for Currency;

    IPoolManager internal immutable manager;

    constructor(IPoolManager manager_) {
        manager = manager_;
    }

    function seed(PoolKey memory key, ModifyLiquidityParams memory params)
        external
    {
        manager.unlock(abi.encode(key, params));
    }

    function unlockCallback(bytes calldata data)
        external
        returns (bytes memory)
    {
        require(msg.sender == address(manager), "only manager");
        (PoolKey memory key, ModifyLiquidityParams memory params) =
            abi.decode(data, (PoolKey, ModifyLiquidityParams));
        (BalanceDelta delta,) = manager.modifyLiquidity(key, params, bytes(""));
        _settle(key.currency0, delta.amount0());
        _settle(key.currency1, delta.amount1());
        return bytes("");
    }

    function _settle(Currency currency, int128 amount) private {
        if (amount < 0) {
            manager.sync(currency);
            IERC20(Currency.unwrap(currency))
                .transfer(address(manager), uint256(uint128(-amount)));
            manager.settle();
        } else if (amount > 0) {
            manager.take(currency, address(this), uint256(uint128(amount)));
        }
    }
}

/// @notice Fork proofs for the unmodified Tycho Router V3 and V4 executor path used by Aqua0.
contract Aqua0TychoBaseForkTest is Test {
    using stdJson for string;

    IPoolManager internal constant BASE_POOL_MANAGER =
        IPoolManager(0x498581fF718922c3f8e6A244956aF099B2652b2b);
    TychoRouterV3 internal constant BASE_TYCHO_ROUTER =
        TychoRouterV3(payable(0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67));
    UniswapV4Executor internal constant BASE_TYCHO_V4_EXECUTOR =
        UniswapV4Executor(0x78db9684220541601E9215bB16b219e5DF6cF0fb);
    uint160 internal constant SQRT_PRICE_1_1 = 1 << 96;

    TychoRouterV3 internal router;
    UniswapV4Executor internal executor;
    Aqua0TychoForkToken internal token0;
    Aqua0TychoForkToken internal token1;
    PoolKey internal key;

    function setUp() public {
        vm.createSelectFork(
            vm.envOr("BASE_RPC_URL", string("https://mainnet.base.org")),
            51_963_803
        );
        assertGt(
            address(BASE_POOL_MANAGER).code.length,
            0,
            "Base PoolManager missing"
        );
        assertGt(
            address(BASE_TYCHO_ROUTER).code.length,
            0,
            "official Tycho Router V3 missing"
        );
        assertGt(
            address(BASE_TYCHO_V4_EXECUTOR).code.length,
            0,
            "official Tycho V4 executor missing"
        );
        assertGt(
            BASE_TYCHO_ROUTER.executorsActivationTimestamp(
                address(BASE_TYCHO_V4_EXECUTOR)
            ),
            0,
            "official Tycho V4 executor is not active"
        );

        Aqua0TychoForkToken tokenA = new Aqua0TychoForkToken("Fork A", "FORKA");
        Aqua0TychoForkToken tokenB = new Aqua0TychoForkToken("Fork B", "FORKB");
        (token0, token1) = address(tokenA) < address(tokenB)
            ? (tokenA, tokenB)
            : (tokenB, tokenA);

        key = PoolKey({
            currency0: Currency.wrap(address(token0)),
            currency1: Currency.wrap(address(token1)),
            fee: 3000,
            tickSpacing: 60,
            hooks: IHooks(address(0))
        });
        BASE_POOL_MANAGER.initialize(key, SQRT_PRICE_1_1);

        Aqua0TychoLiquiditySeeder seeder =
            new Aqua0TychoLiquiditySeeder(BASE_POOL_MANAGER);
        token0.mint(address(seeder), 2e24);
        token1.mint(address(seeder), 2e24);
        seeder.seed(
            key,
            ModifyLiquidityParams({
                tickLower: -887_220,
                tickUpper: 887_220,
                liquidityDelta: 1e24,
                salt: bytes32(0)
            })
        );

        router = BASE_TYCHO_ROUTER;
        executor = BASE_TYCHO_V4_EXECUTOR;
    }

    function testFork_RouterV3ExecutesV4EncodingUsedByAqua0() public {
        uint256 amountIn = 1 ether;
        address trader = makeAddr("trader");
        token0.mint(trader, amountIn);

        bytes memory protocolData = abi.encodePacked(
            address(token0),
            address(token1),
            bytes1(uint8(1)),
            bytes1(uint8(0)),
            address(token1),
            bytes3(uint24(3000)),
            bytes3(uint24(60)),
            address(0),
            bytes2(uint16(0))
        );
        bytes memory swapData =
            abi.encodePacked(address(executor), protocolData);
        ClientFeeParams memory noFee = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            deadline: 0,
            clientSignature: bytes("")
        });

        vm.startPrank(trader);
        token0.approve(address(router), amountIn);
        uint256 amountOut = router.singleSwap(
            amountIn,
            address(token0),
            address(token1),
            0.9 ether,
            0.8 ether,
            trader,
            noFee,
            swapData
        );
        vm.stopPrank();

        assertGe(amountOut, 0.9 ether, "Tycho V4 output too low");
        assertEq(
            token1.balanceOf(trader), amountOut, "receiver output mismatch"
        );
    }

    function testFork_ExistingUniswapXFillerExecutesTychoV4Route() public {
        uint256 amountIn = 1 ether;
        address reactor = makeAddr("reactor");
        address recipient = makeAddr("recipient");
        UniswapXFiller filler =
            new UniswapXFiller(address(router), reactor, address(0));
        token0.mint(address(filler), amountIn);

        bytes memory protocolData = abi.encodePacked(
            address(token0),
            address(token1),
            bytes1(uint8(1)),
            bytes1(uint8(0)),
            address(token1),
            bytes3(uint24(3000)),
            bytes3(uint24(60)),
            address(0),
            bytes2(uint16(0))
        );
        bytes memory swapData =
            abi.encodePacked(address(executor), protocolData);
        ClientFeeParams memory noFee = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            deadline: 0,
            clientSignature: bytes("")
        });
        bytes memory tychoRouterData = abi.encodeWithSelector(
            router.singleSwap.selector,
            amountIn,
            address(token0),
            address(token1),
            0.9 ether,
            0.8 ether,
            address(filler),
            noFee,
            swapData
        );

        OutputToken[] memory outputs = new OutputToken[](1);
        outputs[0] = OutputToken({
            token: address(token1), amount: 0.8 ether, recipient: recipient
        });
        ResolvedOrder[] memory orders = new ResolvedOrder[](1);
        orders[0] = ResolvedOrder({
            info: OrderInfo({
                reactor: reactor,
                swapper: address(0),
                nonce: 0,
                deadline: block.timestamp + 1,
                additionalValidationContract: address(0),
                additionalValidationData: bytes("")
            }),
            input: InputToken({
                token: address(token0), amount: amountIn, maxAmount: amountIn
            }),
            outputs: outputs,
            sig: bytes(""),
            hash: bytes32(0)
        });

        vm.prank(reactor);
        filler.reactorCallback(
            orders, abi.encodePacked(true, true, tychoRouterData)
        );

        assertGe(
            token1.balanceOf(address(filler)),
            0.9 ether,
            "filler output too low"
        );
        assertEq(
            token1.allowance(address(filler), reactor),
            type(uint256).max,
            "reactor output allowance missing"
        );
    }

    struct V3ForkConfig {
        string fixture;
        string rpcEnv;
        string publicRpc;
        IPoolManager poolManager;
        TychoRouterV3 router;
        UniswapV4Executor executor;
        UniswapXFiller filler;
        address reactor;
        address tokenIn;
        address tokenOut;
        uint24 fee;
    }

    address internal constant PERMIT2 =
        0x000000000022D473030F116dDEE9F6B43aC78BA3;
    address internal constant AQUA0_WORKER =
        0xB1C4bA83057A0cB78c69cB1586024f40Fe835382;
    address internal constant FIXTURE_SWAPPER =
        0xe05fcC23807536bEe418f142D19fa0d21BB0cfF7;
    bytes32 internal constant EXECUTOR_ROLE =
        0xd8aa0f3194971a2a116679f7c2090f6939c8d4e01a2a8d7e41d55e5351469e63;
    uint256 internal constant V3_AMOUNT_IN = 1_000_000_000_000;
    uint256 internal constant V3_AMOUNT_OUT = 900_000_000_000;
    int24 internal constant V3_TICK_SPACING = 37;

    /// @dev These four tests retain the older, deployed filler/router pairs as historical plumbing
    ///      checks. They do not prove migration to the current router or execute an Aqua0 hook.
    ///      Orders in test/assets are signed fixtures from uniswapx-sdk 3.1.1. No live broadcast.
    function testFork_BaseV3ReactorExecutesDeployedAqua0Filler() public {
        _proveRealV3(
            V3ForkConfig({
                fixture: "base",
                rpcEnv: "BASE_RPC_URL",
                publicRpc: "https://mainnet.base.org",
                poolManager: IPoolManager(
                    0x498581fF718922c3f8e6A244956aF099B2652b2b
                ),
                router: TychoRouterV3(
                    payable(0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C)
                ),
                executor: UniswapV4Executor(
                    0x78db9684220541601E9215bB16b219e5DF6cF0fb
                ),
                filler: UniswapXFiller(
                    payable(0x9ea548dc4E45Fe0C0Ed616daD53662D158c03fb1)
                ),
                reactor: 0x000000008a8330B5d1F43A62Bf4C673A49f27ba0,
                tokenIn: 0x4200000000000000000000000000000000000006,
                tokenOut: 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913,
                fee: 4_321
            })
        );
    }

    function testFork_ArbitrumV3ReactorExecutesDeployedAqua0Filler() public {
        _proveRealV3(
            V3ForkConfig({
                fixture: "arbitrum",
                rpcEnv: "ARBITRUM_RPC_URL",
                publicRpc: "https://arb1.arbitrum.io/rpc",
                poolManager: IPoolManager(
                    0x360E68faCcca8cA495c1B759Fd9EEe466db9FB32
                ),
                router: TychoRouterV3(
                    payable(0x8A8Ba3973C84252BF7D357E4C0244b7EedB8B658)
                ),
                executor: UniswapV4Executor(
                    0xdb696336F7A5F9048252664A3475C194dAe0e62f
                ),
                filler: UniswapXFiller(
                    payable(0x0C6036274688379E8C4b95F62DDba15B28E2B40B)
                ),
                reactor: 0xB274d5F4b833b61B340b654d600A864fB604a87c,
                tokenIn: 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1,
                tokenOut: 0xaf88d065e77c8cC2239327C5EDb3A432268e5831,
                fee: 4_322
            })
        );
    }

    function testFork_PolygonV3ReactorExecutesDeployedAqua0Filler() public {
        _proveRealV3(
            V3ForkConfig({
                fixture: "polygon",
                rpcEnv: "POLYGON_RPC_URL",
                publicRpc: "https://polygon-bor-rpc.publicnode.com",
                poolManager: IPoolManager(
                    0x67366782805870060151383F4BbFF9daB53e5cD6
                ),
                router: TychoRouterV3(
                    payable(0x0C85409014d6c8cAEF60C837198c931246BD6296)
                ),
                executor: UniswapV4Executor(
                    0x32bB33AfB193e16df121cdc2D87c44f015D325DA
                ),
                filler: UniswapXFiller(
                    payable(0xE2Ad574B76F57c25C5373cD012527cDD89F5CC43)
                ),
                reactor: 0x00000000bAB6E234db8AD638B6A6395b7c499Bc4,
                tokenIn: 0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359,
                tokenOut: 0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619,
                fee: 4_323
            })
        );
    }

    function testFork_RobinhoodV3ReactorExecutesDeployedAqua0Filler() public {
        _proveRealV3(
            V3ForkConfig({
                fixture: "robinhood",
                rpcEnv: "ROBINHOOD_RPC_URL",
                publicRpc: "https://rpc.mainnet.chain.robinhood.com",
                poolManager: IPoolManager(
                    0x8366a39CC670B4001A1121B8F6A443A643e40951
                ),
                router: TychoRouterV3(
                    payable(0x345e48768a65Ae596ac6A2Aee71202753C4866F5)
                ),
                executor: UniswapV4Executor(
                    0xe781c1869c9D8E60dDfcD8F8fb5213Ed8Ad07366
                ),
                filler: UniswapXFiller(
                    payable(0x65Bb39eB5a0cD38C4bE5a7BC219a525202f4F39B)
                ),
                reactor: 0x000000007A1C8e570011EeDF86A2A35593013cBA,
                tokenIn: 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73,
                tokenOut: 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168,
                fee: 4_324
            })
        );
    }

    function _proveRealV3(V3ForkConfig memory cfg) internal {
        vm.createSelectFork(vm.envOr(cfg.rpcEnv, cfg.publicRpc));
        if (block.chainid == 42_161 || block.chainid == 4_663) {
            // Foundry does not emulate the ArbSys precompile on Orbit forks.
            vm.mockCall(
                address(0x64),
                abi.encodeWithSelector(bytes4(keccak256("arbBlockNumber()"))),
                abi.encode(block.number)
            );
        }
        assertGt(address(cfg.poolManager).code.length, 0, "PoolManager missing");
        assertGt(address(cfg.router).code.length, 0, "Tycho Router V3 missing");
        assertGt(
            address(cfg.executor).code.length, 0, "Tycho V4 executor missing"
        );
        assertGt(
            address(cfg.filler).code.length, 0, "Aqua0 UniswapX filler missing"
        );
        assertGt(cfg.reactor.code.length, 0, "V3 reactor missing");
        assertEq(
            address(cfg.filler.reactor()),
            cfg.reactor,
            "filler reactor mismatch"
        );
        assertEq(
            cfg.filler.tychoRouter(),
            address(cfg.router),
            "filler router mismatch"
        );
        assertTrue(
            cfg.filler.hasRole(EXECUTOR_ROLE, AQUA0_WORKER),
            "worker role missing"
        );
        uint256 activation =
            cfg.router.executorsActivationTimestamp(address(cfg.executor));
        assertTrue(
            activation != 0 && activation <= block.timestamp,
            "V4 executor inactive"
        );

        address currency0 =
            cfg.tokenIn < cfg.tokenOut ? cfg.tokenIn : cfg.tokenOut;
        address currency1 =
            cfg.tokenIn < cfg.tokenOut ? cfg.tokenOut : cfg.tokenIn;
        bool zeroForOne = cfg.tokenIn == currency0;
        PoolKey memory v3Key = PoolKey({
            currency0: Currency.wrap(currency0),
            currency1: Currency.wrap(currency1),
            fee: cfg.fee,
            tickSpacing: V3_TICK_SPACING,
            hooks: IHooks(address(0))
        });
        cfg.poolManager.initialize(v3Key, SQRT_PRICE_1_1);

        Aqua0TychoLiquiditySeeder seeder =
            new Aqua0TychoLiquiditySeeder(cfg.poolManager);
        deal(currency0, address(seeder), 2e18, false);
        deal(currency1, address(seeder), 2e18, false);
        seeder.seed(
            v3Key,
            ModifyLiquidityParams({
                tickLower: -887_260,
                tickUpper: 887_260,
                liquidityDelta: 1e18,
                salt: bytes32(0)
            })
        );

        deal(cfg.tokenIn, FIXTURE_SWAPPER, V3_AMOUNT_IN, false);
        vm.prank(FIXTURE_SWAPPER);
        IERC20(cfg.tokenIn).approve(PERMIT2, type(uint256).max);

        bytes memory protocolData = abi.encodePacked(
            cfg.tokenIn,
            cfg.tokenOut,
            bytes1(zeroForOne ? uint8(1) : uint8(0)),
            bytes1(uint8(0)),
            cfg.tokenOut,
            bytes3(cfg.fee),
            bytes3(uint24(V3_TICK_SPACING)),
            address(0),
            bytes2(uint16(0))
        );
        bytes memory swapData =
            abi.encodePacked(address(cfg.executor), protocolData);
        ClientFeeParams memory noFee = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            deadline: 0,
            clientSignature: bytes("")
        });
        bytes memory tychoRouterData = abi.encodeWithSelector(
            cfg.router.singleSwap.selector,
            V3_AMOUNT_IN,
            cfg.tokenIn,
            cfg.tokenOut,
            V3_AMOUNT_OUT,
            V3_AMOUNT_OUT,
            address(cfg.filler),
            noFee,
            swapData
        );

        string memory fixtures = vm.readFile("test/assets/aqua0-v3-orders.json");
        bytes memory encodedOrder =
            fixtures.readBytes(string.concat(".", cfg.fixture, ".order"));
        bytes memory swapperSignature =
            fixtures.readBytes(string.concat(".", cfg.fixture, ".signature"));
        SignedOrder memory order =
            SignedOrder({order: encodedOrder, sig: swapperSignature});

        uint256 inputBefore = IERC20(cfg.tokenIn).balanceOf(FIXTURE_SWAPPER);
        uint256 outputBefore = IERC20(cfg.tokenOut).balanceOf(FIXTURE_SWAPPER);
        vm.prank(AQUA0_WORKER);
        cfg.filler.execute(order, abi.encodePacked(true, true, tychoRouterData));

        assertEq(
            IERC20(cfg.tokenIn).balanceOf(FIXTURE_SWAPPER),
            inputBefore - V3_AMOUNT_IN,
            "input not pulled"
        );
        assertEq(
            IERC20(cfg.tokenOut).balanceOf(FIXTURE_SWAPPER),
            outputBefore + V3_AMOUNT_OUT,
            "V3 reactor did not settle output"
        );
    }
}
