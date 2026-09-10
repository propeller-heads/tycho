# Coding in Rust

## Commands

During development, use simpler commands to help you iterate:

```bash
cargo check --all-features
cargo test
cargo clippy --all-features
```

After a task is done, run `cargo +nightly fmt --all` and the tests that cover what you changed (e.g.
`cargo nextest run -p tycho-indexer`). Save the `/run-ci` skill for when you open a PR or change
something workspace-wide. See `.claude/skills/run-ci/SKILL.md` for the canonical commands.

## Coding Style

### General Rust guidelines

- Prefer `for` loops with mutable accumulators over long iterator chains when clearer
- Use `let...else` for early returns; keep happy path unindented
- No wildcard matches — explicit destructuring catches field changes at compile time
- Newtypes over primitives where the domain warrants it
- Enums for state machines, not boolean flags
- Write efficient code by default — correct algorithm, appropriate data structures, no unnecessary allocations. Profile
  before micro-optimizing.

### Comments

Code should be self-documenting. No commented-out code — delete it. If you need a comment to explain WHAT the code does,
refactor the code instead. Comments explain WHY, not WHAT. Wrap comment lines at 100 characters.

### Docstrings

- Describe **what** the function does and **what the caller can expect** (return values, edge cases, guarantees).
- Do **not** describe where or how the function is called.
- On traits: keep docstrings generic and implementation-agnostic. Document implementation details on the `impl`, not the
  trait method.

### Error handling

- Fail fast with clear, actionable messages.
- Never swallow errors silently.
- Prefer returning results to panicking.
- Include context: what operation failed, what input caused it, what to do next.

### Testing

- **Test behavior, not implementation.** If a refactor breaks tests but not code, the tests were wrong.
- **Test edges and errors.** Empty inputs, boundaries, malformed data — bugs live in edges.
- **Mock boundaries, not logic.** Only mock slow (network, filesystem), non-deterministic (time), or external things you
  don't control. Use the `testing.rs` mocks (`MockBlockchainGateway`, etc.) and `test-utils` feature.
- **Verify tests catch failures.** Break the code, confirm the test fails, then fix.

After every task is done, run `cargo +nightly fmt --all` and the tests covering your change. Then check whether docs
need updating with `/sync-docs`.
