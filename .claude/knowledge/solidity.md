# Coding in Solidity (Foundry)

## Commands

During development, from `crates/tycho-execution/contracts/`:

```bash
forge build                                           # compile
forge test -vvv                                       # run all tests (requires RPC_URL + BASE_RPC_URL)
forge test -vvv --match-test foo                      # run a single test
forge fmt                                             # auto-format
forge fmt --check                                     # format check (CI)
forge lint --report-unused-suppressions --deny notes  # lint (CI)
forge snapshot                                        # update gas snapshots
slither .                                             # static analysis (CI); mise installs Slither
```

forge lint is the primary linter (`[lint]` in `foundry.toml`); Slither runs the checks forge lacks
or handles worse (`slither.config.json`, reasons in `slither.md`). Each check has one owner, so a
finding is suppressed in the tool that reports it.

After a task is done, run `forge fmt` and the tests covering what you changed (e.g.
`forge test --match-contract TychoRouterTest`). Save the `/run-ci` skill for when you open a PR or
change something workspace-wide.

## Coding Style

### Naming

- Private/internal state variables: prefixed `_`: `_feeCalculator`, `_ALLOWED_DUST`
- Input parameters that clash with a state variable: suffixed `_`: `token_`, `amount_`
- Public variables and non-clashing parameters: no prefix or suffix
- Custom errors: contract-prefixed with double underscore: `TychoRouter__EmptySwaps`, `Vault__AmountZero`
- Transient storage slot constants: `keccak256` of a descriptive name string

### Formatting

- `forge fmt` enforced in CI; line length 80 chars

### Lint suppressions

- Suppress a false positive in the tool that owns the check, on the line above it, with the reason
  after the directive: `// forge-lint: disable-next-line(<lint>) <reason>` or
  `// slither-disable-next-line <detector>`. Each tool only reads the line directly above the code,
  so where both apply, or the finding is reported on a later line of a multi-line statement, use
  `// forge-lint: disable-next-item(<lint>)` above the Slither directive

### Assembly

- Avoid assembly (`assembly { ... }`) unless there is no viable alternative — prefer high-level
  Solidity or a library function (OpenZeppelin, solady). A remaining block carries a
  `forge-lint: disable-next-line(inline-assembly)` with its reason

### Executor conventions

When adding an executor, hardcode `TransferType`, `tokenOut`, and `outputToRouter` in
`getTransferData()` — do not make them encodable. Set `outputToRouter = true` if the protocol
sends output to `msg.sender` rather than accepting a receiver param.

`swap()` returns void and must not contain balance tracking, output transfers, or amount
validation — the Dispatcher handles all of this via balance-diff. **Never use pool return
values or callback arguments to determine transfer amounts.**

## Testing

- All tests inherit from `TychoRouterTestSetup.sol`
- Test naming: `test<Description>` in Solidity
- Fork tests require `RPC_URL` (Ethereum mainnet) and `BASE_RPC_URL` env vars

### Cross-language integration tests

Rust encoding tests call `write_calldata_to_file(test_identifier, hex_calldata)`, which appends
`name:hex` lines to `contracts/test/assets/calldata.txt`. Solidity tests read that file via
`loadCallDataFromFile(testName)` and execute the calldata against a mainnet fork. This verifies
Rust-encoded calldata end-to-end.

## Project Config

- Solidity dependencies managed as git submodules under `contracts/lib/` — always clone with
  `--recursive`
