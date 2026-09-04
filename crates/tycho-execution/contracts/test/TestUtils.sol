pragma solidity ^0.8.10;

import "forge-std/Test.sol";

contract TestUtils is Test {
    constructor() {}

    /// Loads the calldata that the rust encoding test named `testName` wrote via
    /// `write_calldata_to_file` (`src/encoding/evm/utils.rs`), one file per test.
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
