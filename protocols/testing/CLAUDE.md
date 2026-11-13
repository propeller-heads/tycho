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
cargo run -- full  --package "ethereum-balancer-v2"          # creation block to latest
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
Strict mode also tests each direction's declared limit (README, "Limits").
`range --report <path>` writes each check's outcome as JSON. See `README.md`, "Strict mode".

### Declaring limits

Agents write these yamls, and a `soft` declaration passes with one line of text, so:

- Choose the kind from the venue's contract, not from test results. Declare `soft` only when the
  contract rejects no input by its amount, with the reason naming that code.
- Never change a kind, or drop a declaration, to make a failing limit check pass. Fix
  `get_limits` or `get_amount_out`, or declare `under_cap` with its evidence and the capacity it
  gives up, or a hard limit's `rounding` with the division that causes it.
- Declare no package-level `soft` when a component has a cap: declare that component's limits on
  the component.
- A PR that changes an existing kind says so in its description.
