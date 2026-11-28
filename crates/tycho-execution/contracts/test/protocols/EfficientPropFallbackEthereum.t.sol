// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IERC20Metadata
} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {IEfficientPropDoor} from "./EfficientPropEthereum.t.sol";

/// @notice Swaps through the PropAMMFallbackExecutor and the fallback router
/// deployed on Ethereum, with an Efficient PropAMM listing door as the pAMM
/// and a Uniswap V3 pool as the fallback, both directions: what a component
/// of `fallback:efficient_prop` encodes to. The router asks the door for its
/// `quote` in the block the swap lands in and fills there unless the pool
/// quotes more; where the door quotes nothing it swaps on the pool.
///
/// Runs against `EFFICIENT_PROP_ETH_RPC` and `EFFICIENT_PROP_ETH_DOOR` as
/// `EfficientPropEthereum.t.sol` does, with `EFFICIENT_PROP_ETH_FALLBACK_POOL`
/// a Uniswap V3 pool of the door's pair that quotes the amounts
/// (`EFFICIENT_PROP_ETH_BASE_IN`, `EFFICIENT_PROP_ETH_QUOTE_IN`) below the
/// door, or a Uniswap V2 pair where `EFFICIENT_PROP_ETH_FALLBACK_V2_FEE_BPS`
/// gives its fee, and is skipped without any of the three. Either branch
/// costs its transaction at least what a `fallback:efficient_prop` swap is
/// charged: tycho-simulation's `FALLBACK_SWAP_GAS`, and the gas estimator's
/// `FALLBACK_ROUTER_OVERHEAD_GAS` and `fallback_quote_gas` for the pool,
/// held against this frame's gas less the refund the branch earns. In a block carrying the
/// maker's update the door fills, at exactly its quote, and the pool is left
/// untouched. With `EFFICIENT_PROP_ETH_NO_UPDATE` true the latest block
/// carries none: the door quotes nothing, the direct path would revert
/// `NoWord`, and the same swap does not revert here, the pool taking the
/// input and paying the receiver.
contract EfficientPropDoorFallbackEthereumForkTest is TestUtils, Constants {
    using SafeERC20 for IERC20;

    /// The `fallback` row `executor_addresses.json` lists for Ethereum.
    address constant EXECUTOR = 0x355d1D7bd40330c235e1132de8D2314b956584c9;
    /// `FallbackProtocol.UniswapV2` and `UniswapV3`, the byte before the
    /// pool in the swap data.
    uint8 constant UNISWAP_V2 = 0;
    uint8 constant UNISWAP_V3 = 1;
    /// `FALLBACK_SWAP_GAS` in tycho-simulation's `efficient_prop/state.rs`,
    /// what the book charges every swap on this path, and what
    /// tycho-execution's gas estimator adds to it: `FALLBACK_ROUTER_OVERHEAD_GAS`
    /// and `fallback_quote_gas`, the router's quote of a Uniswap V2 pair or of
    /// a V3 pool on Ethereum.
    uint256 constant FALLBACK_SWAP_GAS = 35_181;
    uint256 constant FALLBACK_ROUTER_OVERHEAD_GAS = 40_000;
    uint256 constant FALLBACK_V2_QUOTE_GAS = 8000;
    uint256 constant FALLBACK_V3_QUOTE_GAS = 30_000;
    /// The most each branch earned back at its transaction's end in real
    /// transactions on the fork: a swap on the pool (the router's balance of
    /// the input written and cleared, and the guard), and a fill at the door
    /// (the door's balance too).
    uint256 constant POOL_BRANCH_REFUND = 22_235;
    uint256 constant DOOR_BRANCH_REFUND = 39_335;
    /// How far this frame's metering has come in under a real transaction's,
    /// as in `EfficientPropEthereum.t.sol`.
    uint256 constant BAND_UNDER = 600;

    /// A taker no earlier call in a test touched.
    address constant RECEIVER =
        address(uint160(uint256(keccak256("efficient-prop-receiver"))));

    address door;
    address base;
    address quoteToken;
    address pool;
    /// The fee of a Uniswap V2 pair named as the fallback, in bps; zero for a
    /// Uniswap V3 pool.
    uint256 v2FeeBps;
    bool noUpdate;

    function setUp() public {
        string memory rpc = vm.envOr("EFFICIENT_PROP_ETH_RPC", string(""));
        door = vm.envOr("EFFICIENT_PROP_ETH_DOOR", address(0));
        pool = vm.envOr("EFFICIENT_PROP_ETH_FALLBACK_POOL", address(0));
        if (bytes(rpc).length == 0 || door == address(0) || pool == address(0))
        {
            vm.skip(true);
            return;
        }
        noUpdate = vm.envOr("EFFICIENT_PROP_ETH_NO_UPDATE", false);
        v2FeeBps =
            vm.envOr("EFFICIENT_PROP_ETH_FALLBACK_V2_FEE_BPS", uint256(0));
        vm.createSelectFork(rpc);
        base = IEfficientPropDoor(door).BASE();
        IPropAMM.TokenPair memory pair = IPropAMM(door).getPairs()[0];
        quoteToken = pair.token0 == base ? pair.token1 : pair.token0;
    }

    function testSellBaseFillsAtTheDoorOrFallsBack() public {
        _assertSwap(base, quoteToken, vm.envUint("EFFICIENT_PROP_ETH_BASE_IN"));
    }

    function testBuyBaseFillsAtTheDoorOrFallsBack() public {
        _assertSwap(quoteToken, base, vm.envUint("EFFICIENT_PROP_ETH_QUOTE_IN"));
    }

    function _assertSwap(address tokenIn, address tokenOut, uint256 amountIn)
        internal
    {
        // The fallback encoder's layout: the two tokens, the pAMM, then the
        // fallback protocol and its pool.
        bytes memory data = v2FeeBps == 0
            ? abi.encodePacked(tokenIn, tokenOut, door, UNISWAP_V3, pool)
            : abi.encodePacked(
                tokenIn, tokenOut, door, UNISWAP_V2, pool, uint8(v2FeeBps)
            );
        deal(tokenIn, address(this), amountIn);
        deal(tokenOut, RECEIVER, 10 ** IERC20Metadata(tokenOut).decimals());
        uint256 poolHeld = IERC20(tokenIn).balanceOf(pool);

        uint256 beforeSwap = vm.snapshotState();
        (uint256 amountOut, uint256 swapGas) = this.hop(data, amountIn);
        uint256 poolTook = IERC20(tokenIn).balanceOf(pool) - poolHeld;
        vm.revertToState(beforeSwap);

        emit log_named_uint("executor swap gas", swapGas);
        assertGt(amountOut, 0);
        uint256 charged = FALLBACK_SWAP_GAS + FALLBACK_ROUTER_OVERHEAD_GAS
            + (v2FeeBps == 0 ? FALLBACK_V3_QUOTE_GAS : FALLBACK_V2_QUOTE_GAS);
        if (noUpdate) {
            vm.expectRevert();
            IPropAMM(door).quote(tokenIn, tokenOut, amountIn);
            assertEq(poolTook, amountIn);
            assertGe(swapGas + BAND_UNDER, charged + POOL_BRANCH_REFUND);
            return;
        }
        assertEq(amountOut, IPropAMM(door).quote(tokenIn, tokenOut, amountIn));
        assertEq(poolTook, 0);
        assertGe(swapGas + BAND_UNDER, charged + DOOR_BRANCH_REFUND);
    }

    /// The router's hop around the executor, as `Dispatcher` makes it and as
    /// `EfficientPropEthereum.t.sol` meters it: here the executor names the
    /// fallback router as the address the input is sent to.
    function hop(bytes calldata data, uint256 amountIn)
        external
        returns (uint256 amountOut, uint256 swapGas)
    {
        bytes memory answer = Address.functionStaticCall(
            EXECUTOR, abi.encodeCall(IExecutor.getTransferData, (data))
        );
        (, address funded, address tokenIn, address tokenOut,) =
            abi.decode(answer, (uint8, address, address, address, bool));
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(RECEIVER);
        IERC20(tokenIn).safeTransfer(funded, amountIn);

        uint256 gasBefore = gasleft();
        // forge-lint: disable-next-item(low-level-calls) Solidity has no high-level delegatecall.
        (bool success, bytes memory result) = EXECUTOR.delegatecall(
            abi.encodeCall(IExecutor.swap, (amountIn, data, RECEIVER))
        );
        swapGas = gasBefore - gasleft();
        Address.verifyCallResult(success, result);
        amountOut = IERC20(tokenOut).balanceOf(RECEIVER) - balanceBefore;
    }
}
