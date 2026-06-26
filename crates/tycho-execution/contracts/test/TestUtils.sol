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
}

