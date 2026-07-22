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

Execution validation overrides the TychoRouter, FeeCalculator, and protocol executors at simulation
time with the runtime bytecode in `fixtures/*.runtime.json`. These are generated from the
`tycho-execution` contracts, so they must be regenerated whenever those contracts change.

```bash
export RPC_URL=..   # Ethereum mainnet RPC (the router constructor requires a fork)

# Regenerate every fixture from the current contracts
./scripts/update_runtime_bytecode.sh

# Verify the committed fixtures match the current contracts (CI / drift check)
./scripts/update_runtime_bytecode.sh --check
```

The FeeCalculator fixture is a fresh deployment with zero fees, so it is a no-op during simulation
(the router calls it on every swap to read the router fee rate).
