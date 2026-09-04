pragma solidity ^0.8.26;

import {CREATE3} from "@solady/utils/CREATE3.sol";

/// One executor instance: the protocol system it serves, the contract and its constructor arguments.
/// Two protocol systems running one contract with different arguments are two instances.
struct ExecutorDeployment {
    string protocolSystem;
    string contractName;
    bytes constructorArgs;
}

/// The CREATE3 address the calling test contract deploys `protocolSystem`'s executor at. Forge
/// places every test contract at the same address, so this is the address pinned in
/// `config/test_executor_addresses.json`.
function executorAddress(string memory protocolSystem) view returns (address) {
    return CREATE3.predictDeterministicAddress(keccak256(bytes(protocolSystem)));
}

/// Each `<Protocol>Deployment` a test contract inherits registers a hook here from its
/// constructor. `TychoRouterTestSetup.setUp` calls the hooks after forking, so a hook can read the
/// forked chain and deploy what its executor needs.
abstract contract ExecutorDeployments {
    function() internal returns (ExecutorDeployment memory)[] internal
        deploymentHooks;
}
