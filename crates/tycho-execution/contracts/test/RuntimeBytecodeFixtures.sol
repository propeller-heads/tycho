pragma solidity ^0.8.26;

import {CommonBase} from "forge-std/Base.sol";
import {StdCheats} from "forge-std/StdCheats.sol";
import {LibString} from "@solady/utils/LibString.sol";

/// Lists and builds the runtime bytecode fixtures under `protocols/testing/fixtures`, which
/// protocol integration testing plants at simulation time in place of the deployed TychoRouterV3,
/// FeeCalculator and executors. `RuntimeBytecodeFixtures.t.sol` checks the committed fixtures
/// against a fresh build; `script/WriteRuntimeBytecodeFixtures.s.sol` rewrites them.
///
/// Each fixture deploys on a fork of its own, which is why two fixtures may build the same
/// contract. An executor forks the chain it is deployed on, at a block pinned per chain, so a
/// constructor that reads chain state bakes that chain's value in. A fork only has to carry the
/// state the constructor reads: the addresses it checks for code, and any value it stores in an
/// immutable.
abstract contract RuntimeBytecodeFixtures is CommonBase, StdCheats {
    using LibString for string;

    string constant FIXTURES_DIR = "../../../protocols/testing/fixtures/";
    string constant DEPLOYMENTS = "../config/executor_deployments.json";
    string constant FIXTURE_SUFFIX = ".runtime.json";
    string constant WRITE_COMMAND =
        "forge script script/WriteRuntimeBytecodeFixtures.s.sol";

    /// Chain the non-executor fixtures (router, fee calculator) deploy on.
    string constant ETHEREUM = "ethereum";

    address constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;
    /// Stands in for every role admin and fee receiver: those land in storage, not in bytecode.
    address constant PLACEHOLDER_ADMIN = address(1);

    /// Keep aligned with EXECUTOR_ADDRESS in crates/tycho-test/src/execution/encoding.rs.
    /// Self-address immutables must point at the code protocol-testing plants there.
    address constant EXECUTOR_ADDRESS =
        0xaE04CA7E9Ed79cBD988f6c536CE11C621166f41B;

    struct Fixture {
        string name;
        string contractName;
        bytes constructorArgs;
        /// A chain from executor_deployments.json, which is also an alias from `[rpc_endpoints]`
        /// in foundry.toml.
        string chain;
        uint256 blockNumber;
        address deploymentAddress;
    }

    Fixture[] internal _fixtures;
    string private _deployments;

    /// Fills `_fixtures`. Called once, from the test's `setUp` or the script's `run`.
    function _listFixtures() internal {
        _deployments = vm.readFile(DEPLOYMENTS);

        // TychoRouterV3(permit2, feeCalculator, pauserAdmin, unpauserAdmin, executorSetterAdmin,
        // routerFeeSetterAdmin): permit2 is the only immutable, so it has to be the canonical
        // Permit2. feeCalculator only needs deployed code, so Permit2 stands in — simulation
        // points the router at a planted one through storage.
        _contract(
            "TychoRouterV3",
            abi.encode(
                PERMIT2,
                PERMIT2,
                PLACEHOLDER_ADMIN,
                PLACEHOLDER_ADMIN,
                PLACEHOLDER_ADMIN,
                PLACEHOLDER_ADMIN
            )
        );
        // FeeCalculator(routerFeeSetter, routerFeeReceiver): the receiver may not be zero.
        _contract(
            "FeeCalculator", abi.encode(PLACEHOLDER_ADMIN, PLACEHOLDER_ADMIN)
        );

        // Executors take their contract and constructor arguments from executor_deployments.json,
        // the entries the deployment script deploys from. The compiler settings are the test
        // profile's (foundry.toml), not the deployment's, so the bytecode is not the deployed one.
        // Listed by fixture name.
        _executor("BalancerV2", "ethereum", "vm:balancer_v2");
        _executor("BalancerV3", "ethereum", "vm:balancer_v3");
        _executor("Curve", "ethereum", "vm:curve");
        _executor("EkuboV3", "ethereum", "ekubo_v3");
        _executor("EkuboV3Robinhood", "robinhood", "ekubo_v3");
        _executor("FermiSwap", "ethereum", "vm:fermiswap");
        _executor("FluidV1", "ethereum", "fluid_v1");
        _executor("LidoV4", "ethereum", "lido_v4");
        _executor("LiquidityParty", "ethereum", "vm:liquidityparty");
        _executor("LunarBase", "base", "lunarbase");
        _executor("MaverickV2", "ethereum", "vm:maverick_v2");
        _executor("PancakeswapInfinity", "base", "pancakeswap_infinity_cl");
        _executor("RingSwapV2", "ethereum", "ring_swap_v2");
        _executor("RingSwapV2Bsc", "bsc", "ring_swap_v2");
        _executor("Sky", "ethereum", "sky");
        _executor("Slipstreams", "base", "aerodrome_slipstreams");
        _executor("UniswapV2", "ethereum", "uniswap_v2");
        _executor("UniswapV3", "ethereum", "uniswap_v3");
        _executor("UniswapV4", "ethereum", "uniswap_v4");
        // Stands in for whichever hook executor uniswap_v4_hooks is tested against.
        _executor("UniswapV4Angstrom", "ethereum", "uniswap_v4");
        _executor("UniswapV4Robinhood", "robinhood", "uniswap_v4");
    }

    /// Deploys `fixture` on its fork and returns the runtime bytecode.
    function _build(Fixture memory fixture) internal returns (bytes memory) {
        vm.createSelectFork(vm.rpcUrl(fixture.chain), fixture.blockNumber);
        // Each fixture has its own fork, so executors can share the consumer's address.
        // Running the constructor there also makes immutable self-calls target that code.
        deployCodeTo(
            fixture.contractName,
            fixture.constructorArgs,
            fixture.deploymentAddress
        );
        return fixture.deploymentAddress.code;
    }

    /// Block a chain's fixtures fork at. Pinned so a fixture is reproducible, and past the state
    /// the constructors read.
    function _forkBlock(string memory chain) internal pure returns (uint256) {
        // Past block 22090400, where the EtherFi redemption manager went live, so constructors
        // that check it for code can run.
        if (chain.eq(ETHEREUM)) return 23_000_000;
        // No Base, BSC or Robinhood constructor reads chain state yet; these only pin the fork.
        // All are past every address the deployment config names on the chain.
        if (chain.eq("base")) return 46_500_000;
        if (chain.eq("bsc")) return 46_793_446;
        if (chain.eq("robinhood")) return 80_000_000;
        revert(
            string.concat(chain, " has no fork block; add one to _forkBlock")
        );
    }

    /// Lists a fixture built from `contractName` with literal constructor arguments, on Ethereum.
    function _contract(string memory name, bytes memory constructorArgs)
        internal
    {
        _fixtures.push(
            Fixture(
                name,
                name,
                constructorArgs,
                ETHEREUM,
                _forkBlock(ETHEREUM),
                address(bytes20(keccak256(bytes(name))))
            )
        );
    }

    /// Lists an executor fixture, built the way `executor_deployments.json` deploys `protocol` on
    /// `deploymentChain`, on a fork of that chain at its pinned block.
    function _executor(
        string memory name,
        string memory deploymentChain,
        string memory protocol
    ) internal {
        _executor(name, deploymentChain, protocol, _forkBlock(deploymentChain));
    }

    /// Lists an executor fixture that pins a block of its own on `deploymentChain`, for a
    /// constructor that reads state the chain's pinned block does not carry.
    function _executor(
        string memory name,
        string memory deploymentChain,
        string memory protocol,
        uint256 blockNumber
    ) internal {
        string memory key = string.concat(
            "$['", deploymentChain, "']['", protocol, "']"
        );
        require(
            vm.keyExistsJson(_deployments, key),
            string.concat(
                name,
                ": ",
                deploymentChain,
                "/",
                protocol,
                " is missing from executor_deployments.json"
            )
        );
        string memory contractName =
            vm.parseJsonString(_deployments, string.concat(key, ".contract"));
        // `args` holds addresses and unsigned integers, which are all static types, so packing
        // each one into a full word is their constructor encoding. A value that does not parse as
        // uint256 reverts here; one too wide for its parameter reverts in the constructor.
        bytes memory constructorArgs = abi.encodePacked(
            vm.parseJsonUintArray(_deployments, string.concat(key, ".args"))
        );
        _fixtures.push(
            Fixture(
                name,
                contractName,
                constructorArgs,
                deploymentChain,
                blockNumber,
                EXECUTOR_ADDRESS
            )
        );
    }

    function _fixturePath(string memory name)
        internal
        pure
        returns (string memory)
    {
        return string.concat(FIXTURES_DIR, name, FIXTURE_SUFFIX);
    }

    function _isListed(string memory path) internal view returns (bool) {
        for (uint256 i = 0; i < _fixtures.length; i++) {
            if (path.endsWith(
                    string.concat("/", _fixtures[i].name, FIXTURE_SUFFIX)
                )) {
                return true;
            }
        }
        return false;
    }
}
