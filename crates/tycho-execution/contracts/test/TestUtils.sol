pragma solidity ^0.8.10;

import "forge-std/Test.sol";
import {CREATE3} from "@solady/utils/CREATE3.sol";

contract TestUtils is Test {
    constructor() {}

    /// Deploys `contractName` with CREATE3, at an address derived from that name and this test
    /// contract alone: a CREATE2 proxy keyed by `keccak256(contractName)` performs the CREATE, so
    /// the address does not depend on the order in which contracts are deployed, on the
    /// contract's own bytecode, or on where its source sits in the tree. Constructors run
    /// normally; `msg.sender` inside them is the proxy and `address(this)` is the final address.
    function _deployDeterministic(
        string memory contractName,
        bytes memory constructorArgs
    ) internal returns (address) {
        return CREATE3.deployDeterministic(
            abi.encodePacked(vm.getCode(contractName), constructorArgs),
            keccak256(bytes(contractName))
        );
    }

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
}

