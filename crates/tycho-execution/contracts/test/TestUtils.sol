pragma solidity ^0.8.10;

import "forge-std/Test.sol";

contract TestUtils is Test {
    constructor() {}

    function loadCallDataFromFile(string memory testName)
        internal
        view
        returns (bytes memory)
    {
        string memory fileContent = vm.readFile("./test/assets/calldata.txt");
        string[] memory lines = vm.split(fileContent, "\n");

        for (uint256 i = 0; i < lines.length; i++) {
            string[] memory parts = vm.split(lines[i], ":");
            if (
                parts.length >= 2
                    && keccak256(bytes(parts[0])) == keccak256(bytes(testName))
            ) {
                return vm.parseBytes(string.concat("0x", parts[1]));
            }
        }

        revert("Test calldata not found");
    }

    /// The constructor arguments config/executor_deployments.json deploys the executor of
    /// `protocol` with on `chain`, for an executor whose arguments are all addresses. Reverts
    /// when the file has no such entry or an argument is not an address.
    function executorDeploymentArgs(string memory chain, string memory protocol)
        internal
        view
        returns (address[] memory)
    {
        return vm.parseJsonAddressArray(
            vm.readFile("../config/executor_deployments.json"),
            string.concat(".", chain, ".", protocol, ".args")
        );
    }

    /// The address config/test_executor_addresses.json gives the test executor of `protocol`
    /// on `chain`, which the Rust-encoded calldata names. Reverts when the file has no entry.
    function testExecutorAddress(string memory chain, string memory protocol)
        internal
        view
        returns (address)
    {
        return vm.parseJsonAddress(
            vm.readFile("../config/test_executor_addresses.json"),
            string.concat(".", chain, ".", protocol)
        );
    }
}

