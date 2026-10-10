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
    -- Whether the router call returned normally. Not whether the chain kept the result: a caller
    -- quoting the router on chain calls it, reads the return value, then reverts, and that call
    -- returns normally. Read state_committed for that.
    call_success               BOOLEAN NOT NULL,
    -- Whether the chain kept the state changes of the call. NULL on a row indexed before this
    -- column existed, and trades_settled leaves such a row out. Sum volume over that view.
    -- A discarded call is still priced, so quote activity can be read in USD as well.
    state_committed            BOOLEAN,
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
    -- gross_amount_out - expected_amount_out, floored at zero: the surplus the swaps produced.
    -- The router captured it only when positive_slippage_enabled AND NOT positive_slippage_exempt;
    -- otherwise the receiver kept it. Read router_fee_amount for what was actually taken.
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
    -- Whether this trade's client is exempt from positive slippage capture. The client is the
    -- signed client_fee_receiver, or eoa (tx.origin) when that is zero. NULL on a row indexed
    -- before this column existed.
    positive_slippage_exempt   BOOLEAN,
    fee_bps_scale              BIGINT,
    router_fee_amount          NUMERIC(78, 0),
    client_fee_amount          NUMERIC(78, 0),
    n_tokens                   INTEGER NOT NULL,
    n_hops                     INTEGER NOT NULL,
    -- Hop executor addresses. Their protocol names come from the `executors` table at query
    -- time: read the `trade_protocol_systems` view, see executors.sql.
    executors                  TEXT[] NOT NULL,
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
    -- Protocol names come from the `executors` table at query time: read the
    -- `trade_hop_protocols` view, see executors.sql.
    executor        TEXT NOT NULL,
    token_in_index  INTEGER,
    token_out_index INTEGER,
    -- Raw uint24 split share; 0 means "all remaining input".
    split           INTEGER,
    protocol_data   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS trade_hops_trade_idx ON trade_hops (trade_id);

-- One recipient of a FeesTaken event. The router pays a fee into its own vault instead of
-- transferring it, so a row here is a vault credit; read `vault_balances` for the balance.
CREATE TABLE IF NOT EXISTS fees_taken (
    id           TEXT PRIMARY KEY, -- {trade_id}:{index}
    trade_id     TEXT NOT NULL,
    chain        TEXT NOT NULL,
    block_number BIGINT NOT NULL,
    -- From the event, so it is the token the router credited, not the decoded tokenOut.
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
    call_success   BOOLEAN NOT NULL,
    state_committed BOOLEAN
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
    new_value    TEXT,
    -- Denominator the old_value and new_value bps are on, when this event gives its emitter's
    -- generation away; NULL otherwise. The two FeeCalculator generations widened the bps
    -- arguments from uint16 to uint32, so an event carrying one has a different topic in each,
    -- and 10 on the first means the same rate as 100000 on the second.
    bps_scale    BIGINT
);
CREATE INDEX IF NOT EXISTS fee_config_events_emitter_idx ON fee_config_events (chain, emitter, block_number);

-- ERC-6909 vault balance movements on a router.
--
-- Fee credits are absent by design: the router mints those without an event and reports them
-- through FeesTaken, so they arrive in `fees_taken`. Read the `vault_balances` view, which
-- takes both sources; a sum over this table alone runs negative.
CREATE TABLE IF NOT EXISTS vault_transfers (
    id           TEXT PRIMARY KEY, -- {chain}:{tx_hash}:{log_index}
    chain        TEXT NOT NULL,
    block_number BIGINT NOT NULL,
    block_time   TIMESTAMPTZ NOT NULL,
    tx_hash      TEXT NOT NULL,
    log_index    INTEGER NOT NULL,
    -- Each router holds its own vault, so an owner has one balance per router.
    router       TEXT NOT NULL,
    -- msg.sender of the call that moved the balance, which an operator-driven move needs.
    caller       TEXT NOT NULL,
    -- Debited owner, or the zero address on a credit.
    sender       TEXT NOT NULL,
    -- Credited owner, or the zero address on a debit.
    receiver     TEXT NOT NULL,
    token        TEXT NOT NULL,
    amount       NUMERIC(78, 0) NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('credit', 'debit', 'transfer'))
);
CREATE INDEX IF NOT EXISTS vault_transfers_owner_idx
    ON vault_transfers (chain, router, token);
CREATE INDEX IF NOT EXISTS vault_transfers_block_idx ON vault_transfers (chain, block_number);
