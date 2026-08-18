pragma solidity ^0.8.24;

import {IUniversalRouter} from "../interfaces/IUniversalRouter.sol";
import {IPermit2} from "../lib/permit2/src/interfaces/IPermit2.sol";
import {Constants} from "./Constants.sol";
import {Actions} from "../lib/v4-periphery/src/libraries/Actions.sol";
import {PoolKey} from "../lib/v4-core/src/types/PoolKey.sol";
import {IV4Router} from "../lib/v4-periphery/src/interfaces/IV4Router.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Currency} from "../lib/v4-core/src/types/Currency.sol";
import {IHooks} from "../lib/v4-core/src/interfaces/IHooks.sol";
import "forge-std/Test.sol";
import "@permit2/src/interfaces/IAllowanceTransfer.sol";

contract Commands {
    uint256 constant V2_SWAP_EXACT_IN = 0x08;
    uint256 constant V3_SWAP_EXACT_IN = 0x00;
    uint256 constant V4_SWAP = 0x10;
    uint256 constant PERMIT2_PERMIT = 0x0a;
}

// A gas test to compare the gas usage of the UniversalRouter with the TychoRouterV3

contract GasTest is Commands, Test, Constants {
    IUniversalRouter universalRouter = IUniversalRouter(UNIVERSAL_ROUTER);
    IPermit2 permit2 = IPermit2(PERMIT2_ADDRESS);

    function setUp() public {
        uint256 forkBlock = 21817316;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
    }

    function testUniversalRouterUniswapV2() public {
        bool isPermit2 = false;
        uint256 amountIn = 10 ** 18;

        bytes memory commands =
            abi.encodePacked(uint8(Commands.V2_SWAP_EXACT_IN));

        address[] memory path = new address[](2);
        path[0] = WETH_ADDR;
        path[1] = DAI_ADDR;

        bytes[] memory inputs = new bytes[](1);
        inputs[0] = abi.encode(BOB, amountIn, uint256(0), path, isPermit2);

        deal(WETH_ADDR, address(universalRouter), amountIn);
        universalRouter.execute(commands, inputs, block.timestamp + 1000);
    }

    function testUniversalRouterUniswapV2Permit2() public {
        bool isPermit2 = true;
        uint256 amountIn = 10 ** 18;

        bytes memory commands = abi.encodePacked(
            uint8(Commands.PERMIT2_PERMIT), uint8(Commands.V2_SWAP_EXACT_IN)
        );

        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, amountIn);

        address[] memory path = new address[](2);
        path[0] = WETH_ADDR;
        path[1] = DAI_ADDR;

        bytes[] memory inputs = new bytes[](2);
        inputs[0] = abi.encode(permitSingle, signature);
        inputs[1] = abi.encode(ALICE, amountIn, uint256(0), path, isPermit2);

        deal(WETH_ADDR, ALICE, amountIn);

        universalRouter.execute(commands, inputs, block.timestamp + 1000);
    }

    function testUniversalRouterUniswapV3() public {
        bool isPermit2 = false;
        uint256 amountIn = 10 ** 18;

        bytes memory commands =
            abi.encodePacked(uint8(Commands.V3_SWAP_EXACT_IN));

        uint24 poolFee = 3000;
        bytes memory path = abi.encodePacked(WETH_ADDR, poolFee, DAI_ADDR);

        bytes[] memory inputs = new bytes[](1);
        inputs[0] = abi.encode(BOB, amountIn, uint256(0), path, isPermit2);

        deal(WETH_ADDR, address(universalRouter), amountIn);
        universalRouter.execute(commands, inputs, block.timestamp + 1000);
    }

    function testUniversalRouterUniswapV3Permit2() public {
        bool isPermit2 = true;
        uint256 amountIn = 10 ** 18;

        bytes memory commands = abi.encodePacked(
            uint8(Commands.PERMIT2_PERMIT), uint8(Commands.V3_SWAP_EXACT_IN)
        );

        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, amountIn);

        uint24 poolFee = 3000;
        bytes memory path = abi.encodePacked(WETH_ADDR, poolFee, DAI_ADDR);

        bytes[] memory inputs = new bytes[](2);
        inputs[0] = abi.encode(permitSingle, signature);
        inputs[1] = abi.encode(ALICE, amountIn, uint256(0), path, isPermit2);

        deal(WETH_ADDR, ALICE, amountIn);

        universalRouter.execute(commands, inputs, block.timestamp + 1000);
    }

    function testUniversalRouterUniswapV4Permit2() public {
        uint128 amountIn = uint128(100 ether);
        uint128 amountOutMinimum = uint128(0);
        uint256 deadline = block.timestamp + 1000;

        bytes memory commands = abi.encodePacked(
            uint8(Commands.PERMIT2_PERMIT), uint8(Commands.V4_SWAP)
        );

        bytes memory actions = abi.encodePacked(
            uint8(Actions.SWAP_EXACT_IN_SINGLE),
            uint8(Actions.SETTLE_ALL),
            uint8(Actions.TAKE_ALL)
        );

        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(USDE_ADDR, amountIn);

        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(USDE_ADDR),
            currency1: Currency.wrap(USDT_ADDR),
            fee: 100,
            tickSpacing: int24(1),
            hooks: IHooks(address(0))
        });

        bytes[] memory params = new bytes[](3);
        params[0] = abi.encode(
            IV4Router.ExactInputSingleParams({
                poolKey: key,
                zeroForOne: true,
                amountIn: amountIn,
                amountOutMinimum: amountOutMinimum,
                hookData: bytes("")
            })
        );

        params[1] = abi.encode(key.currency0, amountIn);
        params[2] = abi.encode(key.currency1, amountOutMinimum);

        bytes[] memory inputs = new bytes[](2);
        inputs[0] = abi.encode(permitSingle, signature);
        inputs[1] = abi.encode(actions, params);

        deal(USDE_ADDR, ALICE, amountIn);

        universalRouter.execute(commands, inputs, deadline);
    }

    function handlePermit2Approval(address tokenIn, uint256 amount_in)
        internal
        returns (IAllowanceTransfer.PermitSingle memory, bytes memory)
    {
        IERC20(tokenIn).approve(PERMIT2_ADDRESS, amount_in);
        IAllowanceTransfer.PermitSingle memory permitSingle =
            IAllowanceTransfer.PermitSingle({
                details: IAllowanceTransfer.PermitDetails({
                    token: tokenIn,
                    amount: uint160(amount_in),
                    expiration: uint48(block.timestamp + 1 days),
                    nonce: 0
                }),
                spender: UNIVERSAL_ROUTER,
                sigDeadline: block.timestamp + 1 days
            });

        bytes memory signature = signPermit2(permitSingle, ALICE_PK);
        return (permitSingle, signature);
    }

    function signPermit2(
        IAllowanceTransfer.PermitSingle memory permit,
        uint256 privateKey
    ) internal view returns (bytes memory) {
        bytes32 _PERMIT_DETAILS_TYPEHASH = keccak256(
            "PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)"
        );
        bytes32 _PERMIT_SINGLE_TYPEHASH = keccak256(
            "PermitSingle(PermitDetails details,address spender,uint256 sigDeadline)PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)"
        );
        bytes32 domainSeparator = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,uint256 chainId,address verifyingContract)"
                ),
                keccak256("Permit2"),
                block.chainid,
                PERMIT2_ADDRESS
            )
        );
        bytes32 detailsHash =
            keccak256(abi.encode(_PERMIT_DETAILS_TYPEHASH, permit.details));
        bytes32 permitHash = keccak256(
            abi.encode(
                _PERMIT_SINGLE_TYPEHASH,
                detailsHash,
                permit.spender,
                permit.sigDeadline
            )
        );

        bytes32 digest = keccak256(
            abi.encodePacked("\x19\x01", domainSeparator, permitHash)
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(privateKey, digest);

        return abi.encodePacked(r, s, v);
    }
}
