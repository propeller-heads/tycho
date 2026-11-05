# Slither configuration

`slither.config.json` configures the Slither run of the `Slither static analysis` job in
`.github/workflows/ci-foundry.yaml`. Slither only reads JSON, which has no comments, so the reasoning
behind each setting lives here.

forge lint (the `[lint]` section of `foundry.toml`) is the primary linter, and each check has one
owner unless the two tools catch different cases of it. Slither runs the detectors forge does not have and the ones it handles better on this
codebase; the split comes from comparing Slither 0.11.6 with forge 1.9.0 on `src/`.

Run it locally from this directory; Slither picks up `slither.config.json` from the working
directory:

```sh
slither .
```

## `filter_paths`

`(^|/)lib/` drops every finding in a file under a directory named `lib`, which is where Foundry
keeps dependencies (`libs = ['lib']` in `foundry.toml`). Third-party code is not ours to fix: without
the filter, Slither reports over 500 findings there. The pattern is a regex matched against the
file path, and anchoring it to a path separator keeps it from matching names that merely contain
"lib", such as a `src/libraries/` directory.

## `detectors_to_exclude`

These checks belong to forge lint, which has an equivalent lint:

- `arbitrary-send-eth`, `arbitrary-send-erc20`, `arbitrary-send-erc20-permit`, `missing-zero-check`:
  forge recognises OpenZeppelin's and Solady's ETH-sending helpers, and only counts a check that
  guards the send or the assignment.
- `calls-loop`, `delegatecall-loop`, `low-level-calls`, `assembly`, `too-many-digits`, `tx-origin`,
  `return-bomb`, `missing-inheritance`: forge reports the same sites, and reports a call in a loop
  once instead of once per call path.
- `timestamp`, `incorrect-equality`, `unused-state`, `reentrancy-balance`: Slither's taint through
  storage reports zero-sentinel checks, `== 0` early returns and constants read only in slice
  bounds; forge reports none of them.
- `incorrect-shift`, `protected-vars`, `rtlo`, `msg-value-loop`, `mapping-deletion`, `tautology`,
  `incorrect-modifier`, `events-access`, `events-maths`, `incorrect-unary`, `constable-states`,
  `external-function`, `var-read-using-this`, `boolean-equal`, `cyclomatic-complexity`,
  `function-init-state`, `uninitialized-local`, `unprotected-upgrade`, `pragma`: forge's lint
  covers the same check or more of it.

Detectors that report different cases from their forge counterpart run in both tools:
`divide-before-multiply`, `uninitialized-state`, `incorrect-exp`, `weak-prng`, `write-after-write`,
`assert-state-change`, `costly-loop` and `immutable-states`.

## Unused `slither-disable` directives

`warn_unused_ignores` would fail the run on `slither-disable` directives that suppress nothing. It
stays off because Slither also reports the directives in paths that `filter_paths` excludes,
including ones that do suppress a finding there, so every run would fail on the dependencies'
directives ([crytic/slither#3137](https://github.com/crytic/slither/issues/3137)).
