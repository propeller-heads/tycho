pragma solidity ^0.8.26;

import {Script} from "forge-std/Script.sol";
import {RuntimeBytecodeFixtures} from "../test/RuntimeBytecodeFixtures.sol";

/// Rewrites every runtime bytecode fixture from the current contracts:
///
///   forge script script/WriteRuntimeBytecodeFixtures.s.sol
///
/// The deploys stay on local forks, so the script takes no `--broadcast`. Forks through the
/// `[rpc_endpoints]` aliases in `foundry.toml`, one per chain with a listed executor.
contract WriteRuntimeBytecodeFixtures is RuntimeBytecodeFixtures, Script {
    function run() public {
        _listFixtures();
        for (uint256 i = 0; i < _fixtures.length; i++) {
            Fixture memory fixture = _fixtures[i];
            // Reusing the object key overwrites the field, so the JSON holds one entry.
            string memory json = vm.serializeBytes(
                "fixture", "runtimeBytecode", _build(fixture)
            );
            // forge-lint: disable-next-line(unsafe-cheatcode)
            vm.writeFile(_fixturePath(fixture.name), json);
        }
    }
}
