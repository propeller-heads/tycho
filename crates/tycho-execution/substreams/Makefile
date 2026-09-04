# Build, run and sink the router trades substreams.
#
# Requires: rustup target wasm32-unknown-unknown, `substreams` CLI, `substreams-sink-sql`
# (go install github.com/streamingfast/substreams-sink-sql/cmd/substreams-sink-sql@latest),
# SUBSTREAMS_API_TOKEN and per-chain endpoints (see README).
CHAIN ?= ethereum
MANIFEST := tycho-router-trades/chains/$(CHAIN).yaml
DSN ?= psql://tycho:tycho@localhost:5433/router_trades?sslmode=disable
START ?=
STOP ?= +10

.PHONY: build test test-pricing lint fmt protogen pack run setup sink price-once price-loop

# --target-dir keeps the wasm where the manifests expect it even with a global CARGO_TARGET_DIR.
build:
	cargo build --target wasm32-unknown-unknown --release --target-dir target

test:
	cargo test --workspace

test-pricing:
	./scripts/test_pricing.sh

lint:
	cargo +nightly fmt -- --check
	cargo +nightly clippy --all --all-features --all-targets -- -D warnings

fmt:
	cargo +nightly fmt

protogen:
	cd tycho-router-trades && substreams protogen chains/ethereum.yaml --output-path src/pb --exclude-paths="sf/substreams,google,sf/ethereum"

# Packs into target/spkg/, where docker-compose mounts it for the local sinks. release.sh does the
# same for the published packages.
pack: build
	mkdir -p target/spkg
	substreams pack $(MANIFEST) -o target/spkg/$(CHAIN).spkg

pack-all: build
	mkdir -p target/spkg
	for manifest in tycho-router-trades/chains/*.yaml; do \
		chain=$$(basename "$$manifest" .yaml); \
		substreams pack "$$manifest" -o "target/spkg/$$chain.spkg"; \
	done

# Stream decoded trades to the terminal for a block range, e.g.
#   make run CHAIN=ethereum START=25654569 STOP=+100
run: build
	substreams run $(MANIFEST) map_trades $(if $(START),--start-block $(START),) --stop-block $(STOP)

setup:
	substreams-sink-sql setup "$(DSN)" $(MANIFEST)

sink: build
	substreams-sink-sql run "$(DSN)" $(MANIFEST) $(if $(START),$(START):,) --batch-block-flush-interval 100

# Pricing: fills price_*_usd / volume_usd from the Tycho token_price table, anchored to USD
# through the pinned stablecoins in pricing/preferred_tokens.sql.
PSQL_DSN ?= postgres://tycho:tycho@localhost:5433/router_trades

# Applies the migration and the preferred-token list. Needed once before the first pricing pass,
# and after any change to pricing/preferred_tokens.sql.
price-setup:
	psql "$(PSQL_DSN)" -v ON_ERROR_STOP=1 -f pricing/migrate_usd.sql
	psql "$(PSQL_DSN)" -v ON_ERROR_STOP=1 -f pricing/preferred_tokens.sql

price-once:
	psql "$(PSQL_DSN)" -v ON_ERROR_STOP=1 -v chain='$(CHAIN)' \
		-v max_age='$(or $(MAX_AGE),1 hour)' \
		-v price_max_age='$(or $(PRICE_MAX_AGE),3 hours)' -f pricing/price_trades.sql

price-loop:
	DSN="$(PSQL_DSN)" ./scripts/price_trades.sh
