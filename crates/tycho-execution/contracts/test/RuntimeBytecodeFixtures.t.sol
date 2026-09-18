pragma solidity ^0.8.26;

import "./Constants.sol";
import "./TestUtils.sol";

/// Writes and checks the runtime bytecode fixtures under `protocols/testing/fixtures`, which
/// protocol integration testing plants at simulation time in place of the deployed TychoRouterV3,
/// FeeCalculator and executors.
///
/// Every fixture deploys through `_deployDeterministic`, so its address depends on the contract
/// name alone, and neither the fork it runs on nor its position in the list reaches the output.
/// Each fixture deploys on a fork of its own, which is why two fixtures may build the same
/// contract. A fork only has to carry the state the constructor reads: the addresses it checks
/// for code, and any value it stores in an immutable.
///
///   forge test --match-contract RuntimeBytecodeFixtures                    checks the fixtures
///   FIXTURES_WRITE=1 forge test --match-contract RuntimeBytecodeFixtures   rewrites them
contract RuntimeBytecodeFixturesTest is Constants, TestUtils {
    string constant FIXTURES_DIR = "../../../protocols/testing/fixtures/";
    string constant DEPLOYMENTS = "../config/executor_deployments.json";
    string constant FIXTURE_SUFFIX = ".runtime.json";

    /// Fork a fixture deploys on unless it names its own. Past block 22090400, where the EtherFi
    /// redemption manager went live, so constructors that check it for code can run.
    string constant DEFAULT_CHAIN = "mainnet";
    uint256 constant DEFAULT_BLOCK = 23_000_000;

    /// Stands in for every role admin and fee receiver: those land in storage, not in bytecode.
    address constant PLACEHOLDER_ADMIN = address(1);

    struct Fixture {
        string name;
        string contractName;
        bytes constructorArgs;
        /// An alias from `[rpc_endpoints]` in foundry.toml.
        string chain;
        uint256 blockNumber;
    }

    Fixture[] private _fixtures;
    string private _deployments;

    function setUp() public {
        _deployments = vm.readFile(DEPLOYMENTS);

        // TychoRouterV3(permit2, feeCalculator, pauserAdmin, unpauserAdmin, executorSetterAdmin,
        // routerFeeSetterAdmin): permit2 is the only immutable, so it has to be the canonical
        // Permit2. feeCalculator only needs deployed code, so Permit2 stands in — simulation
        // points the router at a planted one through storage.
        _contract(
            "TychoRouterV3",
            abi.encode(
                PERMIT2_ADDRESS,
                PERMIT2_ADDRESS,
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
        // so a fixture is built the way the executor is deployed. Listed by fixture name.
        _executor("BalancerV2", "ethereum", "vm:balancer_v2");
        _executor("BalancerV3", "ethereum", "vm:balancer_v3");
        _executor("Curve", "ethereum", "vm:curve");
        _executor("EkuboV3", "ethereum", "ekubo_v3");
        _executor("EkuboV3Robinhood", "robinhood", "ekubo_v3");
        _executor("FermiSwap", "ethereum", "vm:fermiswap");
        _executor("FluidV1", "ethereum", "fluid_v1");
        _executor("LiquidityParty", "ethereum", "vm:liquidityparty");
        _executor("LunarBase", "base", "lunarbase");
        _executor("MaverickV2", "ethereum", "vm:maverick_v2");
        _executor("RingSwapV2", "ethereum", "ring_swap_v2");
        _executor("Sky", "ethereum", "sky");
        _executor("Slipstreams", "base", "aerodrome_slipstreams");
        _executor("UniswapV2", "ethereum", "uniswap_v2");
        _executor("UniswapV3", "ethereum", "uniswap_v3");
        _executor("UniswapV4", "ethereum", "uniswap_v4");
        // Stands in for whichever hook executor uniswap_v4_hooks is tested against.
        _executor("UniswapV4Angstrom", "ethereum", "uniswap_v4");
    }

    /// Deploys every fixture and writes it, or checks it against the committed file.
    function testFixturesMatchTheContracts() public {
        bool write = vm.envOr("FIXTURES_WRITE", false);

        for (uint256 i = 0; i < _fixtures.length; i++) {
            Fixture memory fixture = _fixtures[i];
            vm.createSelectFork(vm.rpcUrl(fixture.chain), fixture.blockNumber);
            address deployed = _deployDeterministic(
                fixture.contractName, fixture.constructorArgs
            );
            string memory contents = string.concat(
                '{"runtimeBytecode":"', vm.toString(deployed.code), '"}'
            );
            string memory path = _fixturePath(fixture.name);

            if (write) {
                vm.writeFile(path, contents);
            } else {
                assertEq(
                    keccak256(bytes(vm.readFile(path))),
                    keccak256(bytes(contents)),
                    string.concat(
                        fixture.name,
                        FIXTURE_SUFFIX,
                        " is out of date; run FIXTURES_WRITE=1 forge test --match-contract RuntimeBytecodeFixtures"
                    )
                );
            }
        }
    }

    /// A committed fixture this file does not list is never rewritten, so it goes stale without
    /// anything noticing; a listed fixture with no file was never written.
    function testEveryCommittedFixtureIsListed() public view {
        Vm.DirEntry[] memory entries = vm.readDir(FIXTURES_DIR);

        uint256 committed = 0;
        for (uint256 i = 0; i < entries.length; i++) {
            if (!_endsWith(entries[i].path, FIXTURE_SUFFIX)) continue;
            committed++;
            assertTrue(
                _isListed(entries[i].path),
                string.concat(
                    entries[i].path,
                    " is not listed in RuntimeBytecodeFixtures.t.sol"
                )
            );
        }
        for (uint256 i = 0; i < _fixtures.length; i++) {
            assertTrue(
                vm.exists(_fixturePath(_fixtures[i].name)),
                string.concat(
                    _fixtures[i].name,
                    FIXTURE_SUFFIX,
                    " is listed but not committed; run FIXTURES_WRITE=1 forge test --match-contract RuntimeBytecodeFixtures"
                )
            );
        }
        assertEq(committed, _fixtures.length, "fixture count");
    }

    /// Lists a fixture built from `contractName` with literal constructor arguments.
    function _contract(string memory name, bytes memory constructorArgs)
        internal
    {
        _fixtures.push(
            Fixture(name, name, constructorArgs, DEFAULT_CHAIN, DEFAULT_BLOCK)
        );
    }

    /// Lists an executor fixture, built the way `executor_deployments.json` deploys `protocol` on
    /// `deploymentChain`, against the default fork.
    function _executor(
        string memory name,
        string memory deploymentChain,
        string memory protocol
    ) internal {
        _executor(name, deploymentChain, protocol, DEFAULT_CHAIN, DEFAULT_BLOCK);
    }

    /// Lists an executor fixture against a fork of its own, for a constructor that reads state the
    /// default fork does not carry: an address it checks for code, or a value it keeps in an
    /// immutable.
    function _executor(
        string memory name,
        string memory deploymentChain,
        string memory protocol,
        string memory chain,
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
        bytes memory constructorArgs = _constructorArgs(
            vm.parseJson(_deployments, string.concat(key, ".args"))
        );
        _fixtures.push(
            Fixture(name, contractName, constructorArgs, chain, blockNumber)
        );
    }

    /// `executor_deployments.json` holds constructor arguments as addresses and unsigned
    /// integers. `vm.parseJson` returns such an array as ABI-encoded 32-byte words, which is what
    /// a constructor taking those arguments expects once the array header is dropped.
    function _constructorArgs(bytes memory parsedArgs)
        internal
        pure
        returns (bytes memory encoded)
    {
        uint256[] memory words = abi.decode(parsedArgs, (uint256[]));
        for (uint256 i = 0; i < words.length; i++) {
            encoded = abi.encodePacked(encoded, words[i]);
        }
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
            if (_endsWith(
                    path, string.concat("/", _fixtures[i].name, FIXTURE_SUFFIX)
                )) return true;
        }
        return false;
    }

    function _endsWith(string memory text, string memory suffix)
        internal
        pure
        returns (bool)
    {
        bytes memory t = bytes(text);
        bytes memory s = bytes(suffix);
        if (s.length > t.length) return false;
        for (uint256 i = 0; i < s.length; i++) {
            if (t[t.length - s.length + i] != s[i]) return false;
        }
        return true;
    }
}
