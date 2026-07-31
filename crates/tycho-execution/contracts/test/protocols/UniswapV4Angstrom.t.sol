pragma solidity ^0.8.26;

import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "../../src/executors/UniswapV4Executor.sol";
import "../TestUtils.sol";
import "./UniswapV4Utils.sol";
import {Constants} from "../Constants.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";

contract UniswapV4ExecutorExposed is UniswapV4Executor {
    using SafeERC20 for IERC20;

    constructor(IPoolManager _POOL_MANAGER, address _ANGSTROM_HOOK)
        UniswapV4Executor(_POOL_MANAGER, _ANGSTROM_HOOK)
    {}

    function selectAttestation(bytes memory attestationData)
        external
        view
        returns (bytes memory)
    {
        return _selectAttestation(attestationData);
    }

    fallback(
        bytes calldata /*data*/
    )
        external
        returns (bytes memory)
    {
        bytes calldata stripped = msg.data[68:];
        bytes4 sel = bytes4(stripped[:4]);
        address tokenIn;
        if (sel == this.swapExactInput.selector) {
            tokenIn = address(bytes20(stripped[16:36]));
        } else {
            address currency0 = address(bytes20(stripped[16:36]));
            address currency1 = address(bytes20(stripped[48:68]));
            bool zeroForOne = stripped[195] != 0;
            tokenIn = zeroForOne ? currency0 : currency1;
        }

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (tokenIn == address(0)) {
            assert(
                transferType
                    == TransferManager.TransferType.TransferNativeInExecutor
            );
        } else {
            assert(transferType == TransferManager.TransferType.Transfer);
        }
        uint256 amount;
        if (sel == this.swapExactInputSingle.selector) {
            amount = uint128(bytes16(stripped[212:228]));
        } else {
            amount = uint128(bytes16(stripped[52:68]));
        }
        IERC20(tokenIn).safeTransfer(receiver, amount);
        return abi.encode(_unlockCallback(stripped));
    }
}

contract UniswapV4AngstromExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    UniswapV4ExecutorExposed angstromExecutor;
    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 WETH = IERC20(WETH_ADDR);

    function setUp() public {
        // Block must match or be close to attestation block in test_encode_angstrom_grouped_swap
        uint256 forkBlock = 24343805;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        angstromExecutor = new UniswapV4ExecutorExposed(
            IPoolManager(POOL_MANAGER), ANGSTROM_HOOK
        );
    }

    /// @notice Test selecting the correct attestation based on block number
    function testSelectAttestationForBlock() public {
        uint64 block1 = 100;
        bytes memory attestation1 =
            hex"d437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c";
        uint64 block2 = 150;
        bytes memory attestation2 =
            hex"d437f3372f3add2c2bc3245e6bd6f9c202e61bb30c337ddae661e68cc6986c7784cd0aaec455b1f7514b6cd91bff26f002ce7cb42b3b1e2092ea4d1c1fb1e0641cbccfb021b31de25462f25b355cc99c7d509cdc1b";
        uint64 block3 = 250;
        bytes memory attestation3 =
            hex"d437f3372f3add2c2bc3245e6bd6f9c202e61bb3611fb86a0e296b693e974e731d155c438d94a3104cd1a538b335bf2260a2f709300a09ce4a5ef937bd0b4e10b8fac14807257eba12695c16e089a4e54a5494371b";

        bytes memory encodedAttestations = abi.encodePacked(
            block1, attestation1, block2, attestation2, block3, attestation3
        );

        // Test selecting for block 100 (should get attestation 0)
        vm.roll(100);
        bytes memory selected =
            angstromExecutor.selectAttestation(encodedAttestations);
        assertEq(selected, attestation1);

        // Test selecting for block 150 (should get attestation 1)
        vm.roll(150);
        selected = angstromExecutor.selectAttestation(encodedAttestations);
        assertEq(selected, attestation2);

        // Test selecting for block 250 (should get attestation 2)
        vm.roll(250);
        selected = angstromExecutor.selectAttestation(encodedAttestations);
        assertEq(selected, attestation3);

        // Test selecting for block 350 (should return empty bytes)
        vm.roll(350);
        selected = angstromExecutor.selectAttestation(encodedAttestations);
        assertEq(selected, "");
    }

    function testSelectAttestationEmptyAttestations() public view {
        // Encode empty attestations - should return empty bytes
        bytes memory encodedAttestations;
        bytes memory selected =
            angstromExecutor.selectAttestation(encodedAttestations);
        assertEq(selected, "");
    }

    function testSingleSwapAngstrom() public {
        uint256 amountIn = 4160938619;
        deal(USDC_ADDR, address(angstromExecutor), amountIn);
        uint256 poolManagerBalanceBefore = USDC.balanceOf(POOL_MANAGER);

        // Create attestations for multiple blocks
        // The attestation is real return data from the Angstrom attestation API
        bytes memory attestation =
            hex"d437f3372f3add2c2bc3245e6bd6f9c202e61bb324e10ae0affbd0fb7b3622098b00d81ee679dd1adb41c03dda9b50565bddaead205a7c383603a49b4fc576c870d6fac726a007ddc008077f5f942a177cedf3ca1c";

        uint256 currentBlock = block.number;
        uint64 block1 = uint64(currentBlock - 1);
        uint64 block2 = uint64(currentBlock);
        uint64 block3 = uint64(currentBlock + 5);

        bytes memory attestationsWithBlocks = abi.encodePacked(
            block1, attestation, block2, attestation, block3, attestation
        );

        bytes memory firstPool = abi.encodePacked(
            WETH_ADDR,
            uint24(8388608),
            int24(10),
            address(0x0000000aa232009084Bd71A5797d089AA4Edfad4),
            bytes2(uint16(93 * 3)), // hookdata length
            attestationsWithBlocks
        );

        // Encode data with attestations
        bytes memory data =
            abi.encodePacked(USDC_ADDR, WETH_ADDR, true, false, firstPool);

        angstromExecutor.swap(amountIn, data, ALICE);

        assertEq(
            USDC.balanceOf(POOL_MANAGER), poolManagerBalanceBefore + amountIn
        );
        uint256 amountOut = WETH.balanceOf(ALICE);
        assertTrue(amountOut > 0);
    }

    function testSwapWithExpiredAttestations() public {
        // When all attestations are expired, _selectAttestation returns
        // empty bytes. The Angstrom hook accepts empty hook data at this
        // fork block, so the swap succeeds with empty attestations.
        uint256 amountIn = 4160938619;
        deal(USDC_ADDR, address(angstromExecutor), amountIn);

        bytes memory attestation =
            hex"d437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c";

        bytes memory firstPool = abi.encodePacked(
            WETH_ADDR,
            uint24(8388608),
            int24(10),
            address(0x0000000aa232009084Bd71A5797d089AA4Edfad4),
            bytes2(uint16(93)), // hookdata length
            uint64(1), // block number (far in the past)
            attestation
        );

        bytes memory data =
            abi.encodePacked(USDC_ADDR, WETH_ADDR, true, false, firstPool);

        angstromExecutor.swap(amountIn, data, BOB);
        assertGt(WETH.balanceOf(BOB), 0);
    }

    function testGroupedSwapIntegration() public {
        // Load calldata generated by test_encode_angstrom_grouped_swap
        // If you update this data, you must also be sure to update the block number
        // in the setUp() method, since the blocks need to align in order to have
        // valid attestations.
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_angstrom_grouped_swap");

        uint256 amountIn = 1000 * 10 ** 6; // 1000 USDC
        deal(USDC_ADDR, address(angstromExecutor), amountIn);
        uint256 usdcBalanceBeforePool = USDC.balanceOf(POOL_MANAGER);
        uint256 usdcBalanceBeforeExecutor =
            USDC.balanceOf(address(angstromExecutor));
        angstromExecutor.swap(amountIn, protocolData, ALICE);

        // Verify USDC was transferred to pool manager
        assertEq(USDC.balanceOf(POOL_MANAGER), usdcBalanceBeforePool + amountIn);
        // Verify USDC was taken from executor
        assertEq(
            USDC.balanceOf(address(angstromExecutor)),
            usdcBalanceBeforeExecutor - amountIn
        );
        // Verify USDT was received by ALICE
        uint256 amountOut = IERC20(USDT_ADDR).balanceOf(ALICE);
        assertTrue(amountOut > 0);
    }
}
