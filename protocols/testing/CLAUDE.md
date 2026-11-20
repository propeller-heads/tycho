# protocols/testing/

Rust binary (`protocol-testing`) that runs end-to-end integration tests for Substreams protocol
implementations. Spins up a full indexer stack, indexes a block range from each protocol's
`integration_test.tycho.yaml`, then validates resulting state via `tycho-simulation`.

IS in the monorepo `[workspace.members]` (unlike `protocols/substreams/`).

## Running

Requires: `RPC_URL`, `SUBSTREAMS_API_TOKEN`, Postgres (`DATABASE_URL`, default
`postgres://postgres:mypassword@localhost:5431/tycho_indexer_0`).

```bash
cargo run -- range --package "ethereum-balancer-v2"          # block range from yaml
cargo run -- full  --package "ethereum-balancer-v2"          # manifest's initialBlock to latest
cargo run -- range --package "base-aerodrome-slipstreams" --chain base
```

WARNING: each run drops and recreates the database named in `DATABASE_URL`. The spawned
tycho-indexer listens on `TYCHO_SERVER_PORT` / `--tycho-server-port` (default 4242). To run
alongside another Tycho stack, isolate both the database and the port — see `README.md`
("Running alongside another Tycho stack"). The compose db host port is `DB_HOST_PORT`
(default 5431).

Docker Compose is available for isolated runs — see `README.md`.

A `cargo` spawned for the Substreams workspace must drop `RUSTUP_TOOLCHAIN` and the inherited
target-directory variables, or it builds with this crate's toolchain and target dir instead of the
package's; `build_wasm` in `src/utils.rs` shows how.

## Strict mode

A test yaml can opt into `harness: strict`; without it the runner behaves as before. Strict mode
rejects keys the runner does not read, requires every skip to be written as its reason (a string;
`true` and `false` are errors), and reports every failed check of a test, not just the first.
It is not required of any package yet, and new checks go into strict mode only.
Strict mode checks every expected component's balances against the amounts the package's
`balance_lens` reads (README, "Balances"), exactly unless the component declares `tvl_only`, which
the runner verifies (`balances.tvl_only`), and rejects the package-level `skip_balance_check`. A
lens returns 0 only for a token the protocol by design does not account per component; it reads
what the protocol accounts, never whatever matches the indexed number, and an `indexed 0, but …
reads X` failure first means checking whether the substreams emits that token's deltas. A
component's `skip_balance_check` is only for a component no lens can read, such as a singleton
without per-pool amounts, with that reason; never to make a failing check pass. A PR that adds one
says so in its description.
Strict mode also tests each direction's declared limit (README, "Limits").
Strict mode also requires executions to match simulation exactly unless the package, or a
component, declares `execution_tolerance: {bps and/or wei, reason}` (README, "Execution
tolerance"); legacy mode allows 0.5%. A tolerance is the documented bound of a known artifact,
named in the reason with the run that measured it; never widen one to make a check pass, and
declare it on the component that needs it rather than the package.
Strict `full` runs the ladder, the limit checks and the tolerance on every update, for every pool
(unlisted pools get the package's `limits`, so a strict yaml declares a package-level `limits`
default), logs every failed check and keeps going; `full --report` appends each failing update as
a line of JSON (README, "Full mode").
`range --report <path>` writes each check's outcome as JSON. See `README.md`, "Strict mode".

### Declaring limits

Agents write these yamls, and a `soft` declaration passes with one line of text, so:

- Choose the kind from the venue's contract, not from test results. Declare `soft` only when the
  contract rejects no input by its amount, with the reason naming that code.
- Never change a kind, or drop a declaration, to make a failing limit check pass. Fix
  `get_limits` or `get_amount_out`, or declare `under_cap` with its evidence and the capacity it
  gives up. A `get_limits` that cannot report the largest input the venue accepts, such as one
  that inverts a bound on the output with a floor division, is `under_cap`.
- The package-level `limits` default, which strict mode requires, is the claim for every pool of
  the protocol, those no test lists included: `full` and the cluster test check them against it.
  Declare the kind that holds for all of them. A component's own `limits` is for a listed pool
  that differs; an unlisted pool like it is still checked against the default, so say in the PR
  description which kinds of pool differ from it.
- A PR that changes an existing kind says so in its description.
