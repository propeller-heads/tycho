# Protocol Testing

Rust-based integration testing framework for Tycho protocol implementations. See our full
docs [here](https://docs.propellerheads.xyz/tycho/for-dexs/protocol-integration/3.-testing).

## How to Run Locally

```bash
# Ensure PostgreSQL is running or start it via Docker
docker compose up db -d

# Export necessary env vars
export RPC_URL=..
export SUBSTREAMS_API_TOKEN=..

# If you use a local PostgreSQL instance, set the connection string if necessary
# By default, the binary will use `postgres://postgres:mypassword@localhost:5431/tycho_indexer_0`
# export DATABASE_URL=postgresql://postgres:password@localhost:5432/postgres

# Run the tests for a specific package, defined in their integration_test.tycho.yaml file
# This type of tests are constrained to a specific block range defined
cargo run -- range --package "ethereum-balancer-v2"

# To run the full test, that will index from the protocol creation block to the latest:
cargo run -- full --package "ethereum-balancer-v2"

# Run tests on a specific chain. Default is Ethereum.
# Make sure to set the RPC_URL environment variable to match the target network.
cargo run -- range --package "base-aerodrome-slipstreams" --chain base

# Clean up
docker compose down
```

### Running alongside another Tycho stack

The defaults collide with a locally running Tycho stack (Postgres on host port 5431, indexer
on port 4242). Worse, each run drops and recreates the database named in `DATABASE_URL` — so
never point it at a database another instance is using. To run in parallel, isolate all ports:

```bash
# Start a dedicated Postgres on a free host port
DB_HOST_PORT=5433 docker compose up db -d

# Point the test runner at it and pick a free indexer port
export DATABASE_URL=postgres://postgres:mypassword@localhost:5433/tycho_indexer_0
export TYCHO_SERVER_PORT=4243   # or pass --tycho-server-port 4243

cargo run -- range --package "ethereum-balancer-v2"
```

## How to Run with Docker

```bash
# Export necessary env vars
export RPC_URL=..
export SUBSTREAMS_API_TOKEN=..
export PROTOCOLS="ethereum-balancer-v2=weighted_legacy_creation ethereum-ekubo-v2"

# Build both images (test-runner + db) and run the tests. --abort-on-container-exit stops the
# stack when the one-shot test-runner finishes.
docker compose up --build --abort-on-container-exit

# Clean up
docker compose down
```

By default this runs `range` tests. To run the `full` test (continuous sync from the initial block
to the chain tip) set `MODE=full`. In full mode the optional `=` suffix is the start block
(`--initial-block`). Full mode never exits, so omit `--abort-on-container-exit` and tear down
manually:

```bash
export MODE=full
export PROTOCOLS="ethereum-balancer-v2=12345678"
docker compose up --build
```

## Runtime Bytecode Fixtures

Execution validation overrides the TychoRouterV3, FeeCalculator, and protocol executors at simulation
time with the runtime bytecode in `fixtures/*.runtime.json`. These are generated from the
`tycho-execution` contracts, so they must be regenerated whenever those contracts change.

`crates/tycho-execution/contracts/test/RuntimeBytecodeFixtures.sol` lists every fixture and reads
executor constructor arguments from `crates/tycho-execution/config/executor_deployments.json`.
Executor fixtures deploy at `tycho-test`'s `EXECUTOR_ADDRESS`, where execution validation plants
their code, so immutable self-addresses remain callable. The router and fee calculator use
addresses derived from their contract names. Each fixture has its own fork; an executor forks
its deployment chain at a pinned block, unless its constructor needs state from a different block
on that chain.

```bash
cd ../../crates/tycho-execution/contracts
export RPC_URL=..   # and the other [rpc_endpoints] in foundry.toml, one per chain with a listed executor

# Verify the committed fixtures match the current contracts (forge test runs this in CI)
forge test --match-contract RuntimeBytecodeFixtures

# Regenerate every fixture from the current contracts
forge script script/WriteRuntimeBytecodeFixtures.s.sol
```

The FeeCalculator fixture is a fresh deployment with zero fees, so it is a no-op during simulation
(the router calls it on every swap to read the router fee rate).

`UniswapV4Robinhood.runtime.json` builds like the others, from `(robinhood, uniswap_v4)` on a
Robinhood fork, so `forge test` catches drift if the Robinhood executor changes.

## Shared packages and fork aliases

`protocol_packages.json` maps fork names to their Substreams package directories. The Rust runner,
Docker build/filter stages and integration CI read this same file. To register a fork of an existing
package, add its alias here and add its chain manifest and `integration_test_<alias>.tycho.yaml`
(with hyphens replaced by underscores) under the shared package. No Dockerfile or Rust mapping edit
is needed.

Values are runtime directories: `ethereum-uniswap-v4/no-hooks`, for example. Docker builds and
copies the top-level workspace (`ethereum-uniswap-v4`) so its nested manifests and local `target`
directory remain available. Unmapped protocol names resolve to their own directory.

Integration CI executes the selector from the PR's **base commit** and reads the candidate checkout
only as data. Changes to Python selection code are tested separately under `pull_request`, without
secrets or shared caches. The expensive build and execution jobs retain their fork approval gates.

Selection compares the PR head with its merge base:

- Alias mapping, `<alias>.yaml` and `integration_test_<alias>.tycho.yaml` changes test only their
  owning aliases. Removal tests the former package and remaining aliases instead of a deleted job.
- Shared package code, ABIs and Cargo metadata test that package and all its aliases, including
  nested manifest variants.
- Non-Markdown changes under `protocols/testing/` (except the alias map), shared Substreams crates
  and dependencies, the Docker composite action, and the integration workflow test all eligible
  protocols. Substreams lockfile changes that only bump local package versions do not widen scope.
- Root workspace Cargo files and `crates/**` changes do not automatically trigger this expensive
  matrix. Use manual dispatch when a dependency or core change needs protocol integration coverage.
- Documentation-only changes do not schedule integration tests. Manual dispatch accepts either
  protocol names or `protocol=filter` entries and passes filters through unchanged.

The builder resolves aliases once into a directory list; the filter image consumes that list and
needs neither Python nor an extra package installation.

Run the resolution and selection regression tests with Python 3.11 or newer:

```sh
python3 -m unittest discover -s protocols/testing/scripts -p 'test_*.py'
```
