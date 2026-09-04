pragma solidity ^0.8.26;

import "./TychoRouterTestSetup.sol";

/// The Rust encoders read executor addresses from `config/test_executor_addresses.json` and embed
/// them in the calldata fixtures under `test/assets/calldata/` that the solidity integration tests
/// replay against this router. An address configured there that the router does not know is not a
/// visible mistake: every test of that protocol reverts with `Dispatcher__UnapprovedExecutor`
/// instead. This checks the whole config file up front, naming the protocol that is off.
contract ExecutorAddressesTest is TychoRouterTestSetup {
    // Late enough that every venue a conditionally deployed executor needs is on-chain.
    function getForkBlock() public pure override returns (uint256) {
        return 25600000;
    }

    function testConfiguredExecutorsAreRegistered() public view {
        string memory config =
            vm.readFile("../config/test_executor_addresses.json");

        string[] memory chains = vm.parseJsonKeys(config, "$");
        for (uint256 i = 0; i < chains.length; i++) {
            string memory chainKey = string.concat("$['", chains[i], "']");
            string[] memory protocols = vm.parseJsonKeys(config, chainKey);
            for (uint256 j = 0; j < protocols.length; j++) {
                address executor = vm.parseJsonAddress(
                    config, string.concat(chainKey, "['", protocols[j], "']")
                );
                assertTrue(
                    tychoRouter.executorsActivationTimestamp(executor) != 0,
                    string.concat(
                        chains[i],
                        ".",
                        protocols[j],
                        " is not a deployed executor: ",
                        vm.toString(executor)
                    )
                );
            }
        }
    }
}
