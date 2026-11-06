pragma solidity ^0.8.33;

import {Vm} from "forge-std/Vm.sol";
import {
    ZeroSeedLaunchBase,
    ERC20,
    IZeroSeedLaunch,
    ZeroSeedLaunch,
    ZeroSeedLaunchPeriphery,
    ICore,
    PoolBalanceUpdate
} from "../../lib/ekubo-v3/test/extensions/ZeroSeedLaunch.t.sol";
import {Core} from "@ekubo-v3/Core.sol";
import {ILocker} from "@ekubo-v3/interfaces/IFlashAccountant.sol";
import {
    EkuboV3RobinhoodExecutor,
    ZERO_SEED_LAUNCH_ADDRESS
} from "../../src/executors/ekubo_v3/EkuboV3RobinhoodExecutor.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

contract LaunchExecutorLocker is EkuboV3RobinhoodExecutor, ILocker {
    constructor()
        EkuboV3RobinhoodExecutor(0x550cfE69f144327723F818F9Bda950A2f6D41D09)
    {}

    function locked_6416899205(uint256) external {
        address tokenIn = address(bytes20(msg.data[72:92]));
        if (tokenIn != ETH_ADDRESS) {
            ERC20(tokenIn)
                .transfer(msg.sender, uint128(bytes16(msg.data[36:52])));
        }
        bytes memory result = handleCallback(msg.data);
        assembly ("memory-safe") { return(add(result, 32), mload(result)) }
    }

    receive() external payable {}
}

/// Local final-source Core/extension execution. No fork, deployment or live
/// routing claim: the provider's exact hop bytes and callback path execute here.
contract EkuboV3LaunchTest is ZeroSeedLaunchBase {
    LaunchExecutorLocker private executor;

    function setUp() public override {
        super.setUp();
        deployCodeTo(
            "ZeroSeedLaunch.sol:ZeroSeedLaunch",
            abi.encode(CORE),
            ZERO_SEED_LAUNCH_ADDRESS
        );
        executor = new LaunchExecutorLocker();
    }

    function _launch(bool tokenIs0, uint8 decimals_)
        private
        returns (IZeroSeedLaunch.Installation memory inst)
    {
        _quote(tokenIs0, decimals_);
        launch = ZeroSeedLaunch(ZERO_SEED_LAUNCH_ADDRESS);
        periphery = new ZeroSeedLaunchPeriphery(ICore(CORE), launch);
        IZeroSeedLaunch.LaunchParameters memory p = _parameters(
            decimals_ == 18 ? ASK_18_18 : ASK_18_6, REQUESTED_SUPPLY, DECAY_RATE
        );
        vm.prank(creator);
        inst = periphery.create(p, "");
        ERC20(inst.token).approve(address(swapper), type(uint256).max);
        vm.warp(START);
    }

    function _execute(
        IZeroSeedLaunch.Installation memory inst,
        address input,
        uint128 amount
    ) private {
        address output = input == inst.poolKey.token0
            ? inst.poolKey.token1
            : inst.poolKey.token0;
        if (input == address(0)) vm.deal(address(executor), amount);
        else ERC20(input).transfer(address(executor), amount);
        executor.swap(
            amount,
            abi.encodePacked(
                input == address(0) ? ETH_ADDRESS : input,
                output == address(0) ? ETH_ADDRESS : output,
                inst.poolKey.config
            ),
            address(this)
        );
    }

    function _compare(bool tokenIs0, uint8 decimals_) private {
        IZeroSeedLaunch.Installation memory inst = _launch(tokenIs0, decimals_);
        uint128 amount = uint128(10 ** decimals_);
        uint256 snapshot = vm.snapshotState();
        PoolBalanceUpdate expected =
            _buy(inst.poolKey, inst.token, int128(amount), false);
        (uint128 f0, uint128 f1) = launch.creatorFees(inst.poolKey);
        uint128 bought =
            uint128(-_tokenDelta(inst.poolKey, inst.token, expected));
        assertTrue(vm.revertToState(snapshot));
        vm.recordLogs();
        uint256 before = ERC20(inst.token).balanceOf(address(this));
        _execute(inst, activeQuote, amount);
        assertEq(
            ERC20(inst.token).balanceOf(address(this)) - before,
            bought,
            "forwarded buy amount"
        );
        (uint128 actual0, uint128 actual1) = launch.creatorFees(inst.poolKey);
        assertEq(actual0, f0, "token0 fee ledger");
        assertEq(actual1, f1, "token1 fee ledger");
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found;
        for (uint256 i; i < logs.length; ++i) {
            if (
                logs[i].emitter == address(launch)
                    && logs[i].topics[0]
                        == keccak256(
                            "LaunchSwapped(bytes32,address,int128,int128,uint64,uint128,bool)"
                        )
            ) {
                (int128 d0, int128 d1, uint64 rate, uint128 fee, bool feeIs1) = abi.decode(
                    logs[i].data, (int128, int128, uint64, uint128, bool)
                );
                assertEq(d0, expected.delta0());
                assertEq(d1, expected.delta1());
                assertEq(rate, FEE_5);
                assertEq(fee, feeIs1 ? f1 : f0);
                found = true;
            }
        }
        assertTrue(found, "launch fee event");
        vm.warp(START + HALF_LIFE);
        snapshot = vm.snapshotState();
        expected = _sell(inst.poolKey, inst.token, int128(bought), false);
        (f0, f1) = launch.creatorFees(inst.poolKey);
        uint128 quoteOut =
            uint128(-_quoteDelta(inst.poolKey, inst.token, expected));
        assertTrue(vm.revertToState(snapshot));
        before = activeQuote == address(0)
            ? address(this).balance
            : ERC20(activeQuote).balanceOf(address(this));
        _execute(inst, inst.token, bought);
        uint256 afterBalance = activeQuote == address(0)
            ? address(this).balance
            : ERC20(activeQuote).balanceOf(address(this));
        assertEq(
            afterBalance - before,
            quoteOut,
            "forwarded sell amount at decayed fee"
        );
        (actual0, actual1) = launch.creatorFees(inst.poolKey);
        assertEq(actual0, f0);
        assertEq(actual1, f1);
        _assertLedger(inst, activeQuote, amount, quoteOut, MAX_DUST);
    }

    function testLaunchToken0Quote18() public {
        _compare(true, 18);
    }

    function testLaunchToken0Quote6() public {
        _compare(true, 6);
    }

    function testLaunchToken1Native() public {
        _compare(false, 18);
    }

    function testLaunchToken1Quote6() public {
        _compare(false, 6);
    }

    function testLaunchPartialFillRejectedAtomically() public {
        IZeroSeedLaunch.Installation memory inst = _launch(false, 18);
        uint128 amount = uint128(type(int128).max);
        vm.deal(address(executor), amount);
        // This executor settles the requested amount; incomplete fills are
        // rejected by Core's zero-debt invariant. The simulator must reject them.
        vm.expectRevert();
        executor.swap(
            amount,
            abi.encodePacked(ETH_ADDRESS, inst.token, inst.poolKey.config),
            address(this)
        );
        assertEq(ERC20(inst.token).balanceOf(address(this)), 0);
        (uint128 f0, uint128 f1) = launch.creatorFees(inst.poolKey);
        assertEq(f0, 0);
        assertEq(f1, 0);
    }

    function testLaunchRuntimeFixture() public {
        // Constructor has no chain reads or self-address immutable.
        address signed = vm.parseJsonAddress(
            vm.readFile("../config/executor_deployments.json"),
            ".robinhood.ekubo_v3.args[0]"
        );
        bytes memory runtime =
            address(new EkuboV3RobinhoodExecutor(signed)).code;
        string memory path =
            "../../../protocols/testing/fixtures/EkuboV3Robinhood.runtime.json";
        if (vm.envOr("WRITE_LAUNCH_FIXTURE", false)) {
            vm.writeFile(
                path, vm.serializeBytes("fixture", "runtimeBytecode", runtime)
            );
        }
        assertEq(
            runtime, vm.parseJsonBytes(vm.readFile(path), ".runtimeBytecode")
        );
    }
}
