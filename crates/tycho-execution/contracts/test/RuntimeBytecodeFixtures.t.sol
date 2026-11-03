pragma solidity ^0.8.26;

import {Test, Vm} from "forge-std/Test.sol";
import {LibString} from "@solady/utils/LibString.sol";
import {RuntimeBytecodeFixtures} from "./RuntimeBytecodeFixtures.sol";

/// Checks the committed runtime bytecode fixtures against the contracts. This test never writes;
/// `forge script script/WriteRuntimeBytecodeFixtures.s.sol` regenerates the fixtures.
contract RuntimeBytecodeFixturesTest is RuntimeBytecodeFixtures, Test {
    using LibString for string;

    function setUp() public {
        _listFixtures();
    }

    /// Builds every fixture and compares it with the committed file, reporting every stale one.
    function testFixturesMatchTheContracts() public {
        string memory stale;
        for (uint256 i = 0; i < _fixtures.length; i++) {
            Fixture memory fixture = _fixtures[i];
            string memory path = _fixturePath(fixture.name);
            bytes memory built = _build(fixture);
            if (
                !vm.exists(path)
                    || keccak256(
                            vm.parseJsonBytes(
                                vm.readFile(path), ".runtimeBytecode"
                            )
                        ) != keccak256(built)
            ) {
                stale = string.concat(stale, " ", fixture.name);
            }
        }
        assertTrue(
            bytes(stale).length == 0,
            string.concat(
                "fixtures out of date:", stale, "; run ", WRITE_COMMAND
            )
        );
    }

    /// A committed fixture this file does not list is never rewritten, so it goes stale without
    /// anything noticing; a listed fixture with no file was never written.
    function testEveryCommittedFixtureIsListed() public view {
        Vm.DirEntry[] memory entries = vm.readDir(FIXTURES_DIR);

        uint256 committed = 0;
        for (uint256 i = 0; i < entries.length; i++) {
            if (!entries[i].path.endsWith(FIXTURE_SUFFIX)) continue;
            committed++;
            assertTrue(
                _isListed(entries[i].path),
                string.concat(
                    entries[i].path,
                    " is not listed in RuntimeBytecodeFixtures.sol"
                )
            );
        }
        for (uint256 i = 0; i < _fixtures.length; i++) {
            assertTrue(
                vm.exists(_fixturePath(_fixtures[i].name)),
                string.concat(
                    _fixtures[i].name,
                    FIXTURE_SUFFIX,
                    " is listed but not committed; run ",
                    WRITE_COMMAND
                )
            );
        }
        assertEq(committed, _fixtures.length, "fixture count");
    }
}
