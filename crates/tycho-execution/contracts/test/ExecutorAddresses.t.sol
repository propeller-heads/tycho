pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {CREATE3} from "@solady/utils/CREATE3.sol";

/// Checks that every address in `config/test_executor_addresses.json` is where the tests deploy
/// an executor: its own protocol system's for a protocol `config/executor_deployments.json` deploys
/// on that chain, and otherwise its own or that of a protocol deployed on that chain, for a
/// protocol that runs on another protocol's instance (`sushiswap_v2` on `uniswap_v2`'s). A wrong pin
/// otherwise shows up only as `Dispatcher__UnapprovedExecutor` in every test of that protocol.
contract ExecutorAddressesTest is Test {
    function testConfiguredAddressesAreDeployedExecutors() public {
        string memory addresses =
            vm.readFile("../config/test_executor_addresses.json");
        string memory deployments =
            vm.readFile("../config/executor_deployments.json");
        string[] memory chains = vm.parseJsonKeys(addresses, "$");
        for (uint256 i = 0; i < chains.length; i++) {
            string memory chainKey = string.concat("$['", chains[i], "']");
            string[] memory protocols = vm.parseJsonKeys(addresses, chainKey);
            for (uint256 j = 0; j < protocols.length; j++) {
                address pinned = vm.parseJsonAddress(
                    addresses, string.concat(chainKey, "['", protocols[j], "']")
                );
                if (pinned == _executorAddress(protocols[j])) continue;
                bool deployed = vm.keyExistsJson(
                    deployments,
                    string.concat(chainKey, "['", protocols[j], "']")
                );
                assertTrue(
                    !deployed && _isDeployedOn(deployments, chainKey, pinned),
                    string.concat(
                        chains[i],
                        ".",
                        protocols[j],
                        " must be ",
                        vm.toString(_executorAddress(protocols[j])),
                        deployed
                            ? ""
                            : " or the address of a protocol deployed on that chain"
                    )
                );
            }
        }
    }

    /// Forge places every test contract at the same address, so this contract deploys executors
    /// where the router tests do.
    function _executorAddress(string memory protocolSystem)
        internal
        view
        returns (address)
    {
        return CREATE3.predictDeterministicAddress(
            keccak256(bytes(protocolSystem)), address(this)
        );
    }

    function _isDeployedOn(
        string memory deployments,
        string memory chainKey,
        address executor
    ) internal view returns (bool) {
        string[] memory deployed = vm.parseJsonKeys(deployments, chainKey);
        for (uint256 i = 0; i < deployed.length; i++) {
            if (executor == _executorAddress(deployed[i])) return true;
        }
        return false;
    }
}
