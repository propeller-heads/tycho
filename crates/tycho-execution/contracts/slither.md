# Slither configuration

`slither.config.json` configures the Slither run of the `Slither static analysis` job in
`.github/workflows/ci-foundry.yaml`. Slither only reads JSON, which has no comments, so the reasoning
behind each setting lives here.

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

## Unused `slither-disable` directives

`warn_unused_ignores` would fail the run on `slither-disable` directives that suppress nothing. It
stays off because Slither also reports the directives in paths that `filter_paths` excludes,
including ones that do suppress a finding there, so every run would fail on the dependencies'
directives ([crytic/slither#3137](https://github.com/crytic/slither/issues/3137)).
