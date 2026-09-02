// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUnlockCallback} from "@uniswap/v4-core/src/interfaces/callback/IUnlockCallback.sol";
import {Currency, CurrencyLibrary} from "@uniswap/v4-core/src/types/Currency.sol";
import {BalanceDelta, BalanceDeltaLibrary} from "@uniswap/v4-core/src/types/BalanceDelta.sol";
import {PoolKey} from "@uniswap/v4-core/src/types/PoolKey.sol";
import {ModifyLiquidityParams} from "@uniswap/v4-core/src/types/PoolOperation.sol";
import {IHooks} from "@uniswap/v4-core/src/interfaces/IHooks.sol";

import {TychoRouterV3, ClientFeeParams} from "../src/TychoRouterV3.sol";
import {UniswapV4Executor} from "../src/executors/UniswapV4Executor.sol";
import {UniswapXFiller} from "../src/uniswap_x/UniswapXFiller.sol";
import {InputToken, OrderInfo, OutputToken, ResolvedOrder} from "../src/uniswap_x/IStructs.sol";

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

    function seed(PoolKey memory key, ModifyLiquidityParams memory params) external {
        manager.unlock(abi.encode(key, params));
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(msg.sender == address(manager), "only manager");
        (PoolKey memory key, ModifyLiquidityParams memory params) = abi.decode(data, (PoolKey, ModifyLiquidityParams));
        (BalanceDelta delta,) = manager.modifyLiquidity(key, params, bytes(""));
        _settle(key.currency0, delta.amount0());
        _settle(key.currency1, delta.amount1());
        return bytes("");
    }

    function _settle(Currency currency, int128 amount) private {
        if (amount < 0) {
            manager.sync(currency);
            IERC20(Currency.unwrap(currency)).transfer(address(manager), uint256(uint128(-amount)));
            manager.settle();
        } else if (amount > 0) {
            manager.take(currency, address(this), uint256(uint128(amount)));
        }
    }
}

/// @notice Minimal Base-fork proof for the unmodified Tycho Router V3 and V4 executor path used by Aqua0.
contract Aqua0TychoBaseForkTest is Test {
    IPoolManager internal constant BASE_POOL_MANAGER = IPoolManager(0x498581fF718922c3f8e6A244956aF099B2652b2b);
    TychoRouterV3 internal constant BASE_TYCHO_ROUTER =
        TychoRouterV3(payable(0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C));
    UniswapV4Executor internal constant BASE_TYCHO_V4_EXECUTOR =
        UniswapV4Executor(0x78db9684220541601E9215bB16b219e5DF6cF0fb);
    uint160 internal constant SQRT_PRICE_1_1 = 1 << 96;

    TychoRouterV3 internal router;
    UniswapV4Executor internal executor;
    Aqua0TychoForkToken internal token0;
    Aqua0TychoForkToken internal token1;
    PoolKey internal key;

    function setUp() public {
        vm.createSelectFork(vm.envOr("BASE_RPC_URL", string("https://mainnet.base.org")), 50_550_000);
        assertGt(address(BASE_POOL_MANAGER).code.length, 0, "Base PoolManager missing");
        assertGt(address(BASE_TYCHO_ROUTER).code.length, 0, "official Tycho Router V3 missing");
        assertGt(address(BASE_TYCHO_V4_EXECUTOR).code.length, 0, "official Tycho V4 executor missing");
        assertGt(
            BASE_TYCHO_ROUTER.executorsActivationTimestamp(address(BASE_TYCHO_V4_EXECUTOR)),
            0,
            "official Tycho V4 executor is not active"
        );

        Aqua0TychoForkToken tokenA = new Aqua0TychoForkToken("Fork A", "FORKA");
        Aqua0TychoForkToken tokenB = new Aqua0TychoForkToken("Fork B", "FORKB");
        (token0, token1) = address(tokenA) < address(tokenB) ? (tokenA, tokenB) : (tokenB, tokenA);

        key = PoolKey({
            currency0: Currency.wrap(address(token0)),
            currency1: Currency.wrap(address(token1)),
            fee: 3000,
            tickSpacing: 60,
            hooks: IHooks(address(0))
        });
        BASE_POOL_MANAGER.initialize(key, SQRT_PRICE_1_1);

        Aqua0TychoLiquiditySeeder seeder = new Aqua0TychoLiquiditySeeder(BASE_POOL_MANAGER);
        token0.mint(address(seeder), 2e24);
        token1.mint(address(seeder), 2e24);
        seeder.seed(
            key,
            ModifyLiquidityParams({tickLower: -887_220, tickUpper: 887_220, liquidityDelta: 1e24, salt: bytes32(0)})
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
        bytes memory swapData = abi.encodePacked(address(executor), protocolData);
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
            amountIn, address(token0), address(token1), 0.9 ether, 0.8 ether, trader, noFee, swapData
        );
        vm.stopPrank();

        assertGe(amountOut, 0.9 ether, "Tycho V4 output too low");
        assertEq(token1.balanceOf(trader), amountOut, "receiver output mismatch");
    }

    function testFork_ExistingUniswapXFillerExecutesTychoV4Route() public {
        uint256 amountIn = 1 ether;
        address reactor = makeAddr("reactor");
        address recipient = makeAddr("recipient");
        UniswapXFiller filler = new UniswapXFiller(address(router), reactor, address(0));
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
        bytes memory swapData = abi.encodePacked(address(executor), protocolData);
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
        outputs[0] = OutputToken({token: address(token1), amount: 0.8 ether, recipient: recipient});
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
            input: InputToken({token: address(token0), amount: amountIn, maxAmount: amountIn}),
            outputs: outputs,
            sig: bytes(""),
            hash: bytes32(0)
        });

        vm.prank(reactor);
        filler.reactorCallback(orders, abi.encodePacked(true, true, tychoRouterData));

        assertGe(token1.balanceOf(address(filler)), 0.9 ether, "filler output too low");
        assertEq(token1.allowance(address(filler), reactor), type(uint256).max, "reactor output allowance missing");
    }
}
