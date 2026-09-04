-- Schema for substreams-sink-sql. Every table carries `chain` so multiple sinks (one per
-- chain) can share one database; `id` is unique across chains.

CREATE TABLE IF NOT EXISTS trades (
    id                         TEXT PRIMARY KEY, -- {chain}:{tx_hash}:{call_index}
    chain                      TEXT NOT NULL,
    block_number               BIGINT NOT NULL,
    block_time                 TIMESTAMPTZ NOT NULL,
    tx_hash                    TEXT NOT NULL,
    tx_index                   INTEGER NOT NULL,
    call_index                 INTEGER NOT NULL,
    tx_success                 BOOLEAN NOT NULL,
    call_success               BOOLEAN NOT NULL,
    router                     TEXT NOT NULL,
    router_version             TEXT NOT NULL,
    strategy                   TEXT NOT NULL,
    funding                    TEXT NOT NULL,
    eoa                        TEXT NOT NULL,
    msg_sender                 TEXT NOT NULL,
    receiver                   TEXT NOT NULL,
    token_in                   TEXT NOT NULL,
    token_out                  TEXT NOT NULL,
    amount_in                  NUMERIC(78, 0) NOT NULL,
    expected_amount_out        NUMERIC(78, 0),
    min_amount_out             NUMERIC(78, 0) NOT NULL,
    -- (expected - min) / expected in basis points; NULL when expected is absent or zero.
    slippage_tolerance_bps     NUMERIC(20, 4),
    amount_out                 NUMERIC(78, 0),
    -- amount_out + total fees taken, i.e. what the swaps produced before fee deduction.
    gross_amount_out           NUMERIC(78, 0),
    positive_slippage          NUMERIC(78, 0),
    native_value               NUMERIC(78, 0) NOT NULL,
    gas_used                   BIGINT NOT NULL,
    revert_selector            TEXT,
    revert_reason              TEXT,
    client_fee_bps             BIGINT,
    client_fee_receiver        TEXT,
    max_client_contribution    NUMERIC(78, 0),
    client_fee_deadline        NUMERIC(78, 0),
    has_client_signature       BOOLEAN,
    fee_calculator             TEXT,
    router_fee_on_output_bps   BIGINT,
    router_fee_on_client_fee_bps BIGINT,
    custom_fee_on_output       BOOLEAN,
    custom_fee_on_client_fee   BOOLEAN,
    positive_slippage_enabled  BOOLEAN,
    fee_bps_scale              BIGINT,
    router_fee_amount          NUMERIC(78, 0),
    client_fee_amount          NUMERIC(78, 0),
    n_tokens                   INTEGER NOT NULL,
    n_hops                     INTEGER NOT NULL,
    executors                  TEXT[] NOT NULL,
    protocol_systems           TEXT[] NOT NULL,
    watermark                  TEXT,
    wrap_eth                   BOOLEAN NOT NULL,
    unwrap_eth                 BOOLEAN NOT NULL,
    -- Filled after ingestion by pricing/price_trades.sql. USD, price of one whole token. NULL
    -- until priced.
    price_in_usd               DOUBLE PRECISION,
    price_out_usd              DOUBLE PRECISION,
    decimals_in                INTEGER,
    decimals_out               INTEGER,
    -- The trade valued from one side, in USD. Which side and on what basis is price_source:
    -- <in|out>_<stable|preferred|tycho>. Only *_stable rows are valid for a trade older than the
    -- pricing window, because Tycho holds no price history; filter on price_source before
    -- summing.
    volume_usd                 DOUBLE PRECISION,
    price_source               TEXT,
    -- USD price of the chain's native token at pricing time, the anchor every value above went
    -- through. Kept so a row can be re-checked later.
    native_usd                 DOUBLE PRECISION,
    priced_at                  TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS trades_chain_block_idx ON trades (chain, block_number);
CREATE INDEX IF NOT EXISTS trades_block_time_idx ON trades (block_time);
CREATE INDEX IF NOT EXISTS trades_token_in_idx ON trades (chain, token_in);
CREATE INDEX IF NOT EXISTS trades_token_out_idx ON trades (chain, token_out);
CREATE INDEX IF NOT EXISTS trades_eoa_idx ON trades (chain, eoa);
CREATE INDEX IF NOT EXISTS trades_client_idx ON trades (chain, client_fee_receiver);
CREATE INDEX IF NOT EXISTS trades_unpriced_idx ON trades (block_time)
    WHERE priced_at IS NULL AND tx_success AND call_success;

CREATE TABLE IF NOT EXISTS trade_hops (
    id              TEXT PRIMARY KEY, -- {trade_id}:{hop_index}
    trade_id        TEXT NOT NULL,
    chain           TEXT NOT NULL,
    block_number    BIGINT NOT NULL,
    hop_index       INTEGER NOT NULL,
    executor        TEXT NOT NULL,
    protocol_systems TEXT[] NOT NULL,
    token_in_index  INTEGER,
    token_out_index INTEGER,
    -- Raw uint24 split share; 0 means "all remaining input".
    split           INTEGER,
    protocol_data   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS trade_hops_trade_idx ON trade_hops (trade_id);
CREATE INDEX IF NOT EXISTS trade_hops_protocol_idx ON trade_hops USING GIN (protocol_systems);

CREATE TABLE IF NOT EXISTS fees_taken (
    id           TEXT PRIMARY KEY, -- {trade_id}:{index}
    trade_id     TEXT NOT NULL,
    chain        TEXT NOT NULL,
    block_number BIGINT NOT NULL,
    token        TEXT NOT NULL,
    recipient    TEXT NOT NULL,
    amount       NUMERIC(78, 0) NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('router', 'client'))
);
CREATE INDEX IF NOT EXISTS fees_taken_trade_idx ON fees_taken (trade_id);
CREATE INDEX IF NOT EXISTS fees_taken_recipient_idx ON fees_taken (chain, recipient);

CREATE TABLE IF NOT EXISTS router_call_errors (
    id             TEXT PRIMARY KEY, -- {chain}:{tx_hash}:{call_index}
    chain          TEXT NOT NULL,
    block_number   BIGINT NOT NULL,
    block_time     TIMESTAMPTZ NOT NULL,
    tx_hash        TEXT NOT NULL,
    tx_index       INTEGER NOT NULL,
    call_index     INTEGER NOT NULL,
    router         TEXT NOT NULL,
    router_version TEXT NOT NULL,
    stage          TEXT NOT NULL,
    error          TEXT NOT NULL,
    tx_success     BOOLEAN NOT NULL,
    call_success   BOOLEAN NOT NULL
);
CREATE INDEX IF NOT EXISTS router_call_errors_chain_block_idx
    ON router_call_errors (chain, block_number);

CREATE TABLE IF NOT EXISTS fee_config_events (
    id           TEXT PRIMARY KEY, -- {chain}:{tx_hash}:{log_index}
    chain        TEXT NOT NULL,
    block_number BIGINT NOT NULL,
    block_time   TIMESTAMPTZ NOT NULL,
    tx_hash      TEXT NOT NULL,
    log_index    INTEGER NOT NULL,
    emitter      TEXT NOT NULL,
    event        TEXT NOT NULL,
    client       TEXT,
    old_value    TEXT,
    new_value    TEXT
);
CREATE INDEX IF NOT EXISTS fee_config_events_emitter_idx ON fee_config_events (chain, emitter, block_number);
