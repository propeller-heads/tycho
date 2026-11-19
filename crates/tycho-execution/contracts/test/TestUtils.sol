pragma solidity ^0.8.10;

import "forge-std/Test.sol";

contract TestUtils is Test {
    constructor() {}

    /// The calldata the Rust test `testName` wrote with `write_calldata_to_file`.
    function loadCallDataFromFile(string memory testName)
        internal
        view
        returns (bytes memory)
    {
        string memory hexCallData = vm.readFile(
            string.concat("./test/assets/calldata/", testName, ".hex")
        );
        return vm.parseBytes(string.concat("0x", hexCallData));
    }
}
