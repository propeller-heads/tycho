-- Views derived from `trades`.
--
-- Every sink and the pricer apply this file on start, so it has to be idempotent and it has to
-- tolerate a database whose sinks have not created `trades` yet.
--
--   psql "$DSN" -f views.sql
BEGIN;

-- The same lock executors.sql takes, so the containers wait for each other instead of racing on
-- the same DDL.
DO $$ BEGIN PERFORM pg_advisory_xact_lock(6045170023); END $$;

DO $$
BEGIN
    IF to_regclass('public.trades') IS NULL THEN
        RAISE NOTICE 'trades table does not exist yet, skipping the views';
        RETURN;
    END IF;

    -- Set by the package since the release that reads `state_reverted` on the call. An existing
    -- database was created before it, and `schema.sql` only creates a table it does not find,
    -- so the column is added here.
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS state_committed BOOLEAN;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS router_positive_slippage_bps BIGINT;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS client_positive_slippage_share_bps BIGINT;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS positive_slippage_cap_bps BIGINT;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS token_pair_fee BOOLEAN;
    ALTER TABLE IF EXISTS fee_config_events ADD COLUMN IF NOT EXISTS token0 TEXT;
    ALTER TABLE IF EXISTS fee_config_events ADD COLUMN IF NOT EXISTS token1 TEXT;
    ALTER TABLE router_call_errors ADD COLUMN IF NOT EXISTS state_committed BOOLEAN;

    -- Looking up the trades of one transaction by hash, which no other index serves.
    CREATE INDEX IF NOT EXISTS trades_tx_idx ON trades (chain, tx_hash);

    -- The trades to sum volume over: one row per swap the chain actually kept.
    --
    -- A caller can quote the router on chain by calling it and reverting, and only then execute
    -- the route it liked. The router call of such a probe returns normally, so it carries
    -- tx_success and call_success like any fill and the package stores it as a trade. On
    -- robinhood about 9% of the stored trades are probes, and they hold 15.6% of the USD volume.
    --
    -- `state_committed` separates the two exactly: the chain threw away the state changes of a
    -- probe. A row indexed before that column existed carries NULL and stays out, because
    -- nothing in it says the call was kept; re-indexing the chain settles it either way.
    --
    -- subsidized_trades reads this view, so it goes first; it is recreated further down.
    DROP VIEW IF EXISTS subsidized_trades;
    DROP VIEW IF EXISTS trades_settled;
    CREATE VIEW trades_settled AS
    SELECT t.*
    FROM trades t
    WHERE t.tx_success
      AND t.call_success
      AND t.state_committed;
END $$;

DO $$
BEGIN
    IF to_regclass('public.vault_transfers') IS NULL OR to_regclass('public.trades') IS NULL THEN
        RAISE NOTICE 'vault_transfers or trades does not exist yet, skipping the vault views';
        RETURN;
    END IF;

    -- Every movement of a vault balance, one row per owner and side.
    --
    -- The router keeps token balances of its own (ERC-6909) and credits a fee recipient there
    -- instead of transferring, so router revenue accrues in the vault until someone withdraws
    -- it. Two sources are needed to see a balance, because the two kinds of movement report
    -- themselves differently:
    --   * `vault_transfers` holds what the ERC-6909 Transfer event carries: deposits,
    --     withdrawals, moves between owners, and the balance a swap funded from or paid into
    --     the vault.
    --   * the fee credit emits no Transfer, on purpose. `Vault._creditVaultForFees` mints
    --     without an event to save gas, and `_takeFees` emits FeesTaken instead, which lands in
    --     `fees_taken`. Those rows are the `fee` source below.
    DROP VIEW IF EXISTS vault_balances;
    DROP VIEW IF EXISTS vault_flows;
    CREATE VIEW vault_flows AS
    SELECT v.chain, v.router, v.receiver AS owner, v.token, v.block_number, v.block_time,
           v.tx_hash, v.amount AS delta, v.kind AS source
    FROM vault_transfers v
    WHERE v.kind IN ('credit', 'transfer')
    UNION ALL
    SELECT v.chain, v.router, v.sender, v.token, v.block_number, v.block_time,
           v.tx_hash, -v.amount, v.kind
    FROM vault_transfers v
    WHERE v.kind IN ('debit', 'transfer')
    UNION ALL
    SELECT f.chain, t.router, f.recipient, f.token, f.block_number, t.block_time,
           t.tx_hash, f.amount, 'fee'
    FROM fees_taken f
    JOIN trades t ON t.id = f.trade_id
    WHERE f.amount > 0
      AND t.tx_success
      AND t.call_success
      AND t.state_committed;

    -- What each owner holds in each router's vault, and how much of it fees put there.
    --
    -- A balance of zero means the owner withdrew everything, and stays in so the fees an owner
    -- collected over time remain visible. `balance` is what the router's balanceOf returns, so
    -- it can be checked against the chain.
    --
    -- A negative balance is impossible on chain and means rows are missing here. Expect one on
    -- a chain still holding trades indexed before `state_committed` existed: the withdrawal is
    -- in `vault_transfers`, and the fee credit that paid for it is on a trade this view cannot
    -- count. Re-indexing the chain clears it.
    CREATE VIEW vault_balances AS
    SELECT chain, router, owner, token,
           sum(delta) AS balance,
           sum(delta) FILTER (WHERE source = 'fee') AS credited_as_fees,
           count(*) AS n_flows,
           max(block_number) AS last_block,
           max(block_time) AS last_change
    FROM vault_flows
    GROUP BY 1, 2, 3, 4;

    -- vault_balances valued in USD, stored so a dashboard or an alert reads a few thousand rows
    -- instead of summing every flow. The fee branch of vault_flows joins each credit to its
    -- trade, so the view reads all of `trades` and takes longer than Grafana waits for a query.
    --
    -- A table rather than a materialized view: this file drops and recreates vault_balances on
    -- every container start, which a materialized view on top of it would block. The pricer
    -- refreshes it with refresh_vault_balances_snapshot(); `refreshed_at` says how old it is.
    -- CREATE TABLE IF NOT EXISTS leaves an existing table as it is, so a new column needs an
    -- ALTER TABLE here, like the ones on `trades` above.
    CREATE TABLE IF NOT EXISTS vault_balances_snapshot (
        chain            TEXT             NOT NULL,
        router           TEXT             NOT NULL,
        owner            TEXT             NOT NULL,
        token            TEXT             NOT NULL,
        balance          NUMERIC          NOT NULL,
        credited_as_fees NUMERIC,
        n_flows          BIGINT           NOT NULL,
        last_block       BIGINT           NOT NULL,
        last_change      TIMESTAMPTZ      NOT NULL,
        -- From preferred_tokens, else from a priced trade. NULL when neither knows the token.
        decimals         INTEGER,
        -- The token's most recent settled price: a spot estimate, not a mark. NULL for a token
        -- no priced trade has produced, and balance_usd with it.
        price_usd        DOUBLE PRECISION,
        balance_usd      DOUBLE PRECISION,
        refreshed_at     TIMESTAMPTZ      NOT NULL,
        PRIMARY KEY (chain, router, owner, token)
    );

    -- Replaces the snapshot in one transaction. DELETE rather than TRUNCATE, so a reader keeps
    -- the previous snapshot until this one commits instead of waiting for the lock.
    --
    -- plpgsql, because its body is checked only when it runs: preferred_tokens is created by
    -- the pricer, and a sink can apply this file before the pricer has started.
    CREATE OR REPLACE FUNCTION refresh_vault_balances_snapshot() RETURNS VOID
    LANGUAGE plpgsql AS $fn$
    BEGIN
        DELETE FROM vault_balances_snapshot;
        INSERT INTO vault_balances_snapshot (
            chain, router, owner, token, balance, credited_as_fees, n_flows, last_block,
            last_change, decimals, price_usd, balance_usd, refreshed_at
        )
        WITH vb AS (
            SELECT * FROM vault_balances
        ),
        -- One price per held token, from the token_out index rather than a pass over trades.
        px AS (
            SELECT DISTINCT ON (t.chain, t.token_out)
                   t.chain, t.token_out AS token, t.price_out_usd, t.decimals_out
            FROM trades_settled t
            WHERE t.price_out_usd IS NOT NULL
              AND (t.chain, t.token_out) IN (SELECT chain, token FROM vb)
            ORDER BY t.chain, t.token_out, t.block_time DESC
        )
        SELECT vb.chain, vb.router, vb.owner, vb.token, vb.balance, vb.credited_as_fees,
               vb.n_flows, vb.last_block, vb.last_change,
               coalesce(pt.decimals, px.decimals_out),
               px.price_out_usd,
               vb.balance / power(10, coalesce(pt.decimals, px.decimals_out))
                   * px.price_out_usd,
               now()
        FROM vb
          LEFT JOIN preferred_tokens pt
            ON pt.chain = vb.chain AND lower(pt.address) = lower(vb.token)
          LEFT JOIN px ON px.chain = vb.chain AND px.token = vb.token;
    END
    $fn$;
END $$;

DO $$
BEGIN
    IF to_regclass('public.subsidies') IS NULL OR to_regclass('public.trades') IS NULL THEN
        RAISE NOTICE 'subsidies or trades does not exist yet, skipping the subsidy view';
        RETURN;
    END IF;

    -- Each subsidy the payer paid, next to the settled trade it was paid on. `subsidy_usd` is
    -- known when the subsidy was paid in the trade's output token, which is the price the pricer
    -- has; a subsidy on an earlier hop is paid in that hop's token and stays unpriced here.
    DROP VIEW IF EXISTS subsidized_trades;
    CREATE VIEW subsidized_trades AS
    SELECT s.chain, s.block_number, s.block_time, s.tx_hash, s.trade_id, s.payer, s.nonce,
           s.bound_to, s.token AS subsidy_token, s.amount AS subsidy_amount,
           CASE
               WHEN s.token = t.token_out AND t.price_out_usd IS NOT NULL
               THEN s.amount / power(10::numeric, t.decimals_out) * t.price_out_usd
           END AS subsidy_usd,
           t.eoa, t.msg_sender, t.receiver, t.token_in, t.token_out, t.amount_in, t.amount_out,
           t.volume_usd, t.price_source
    FROM subsidies s
    JOIN trades_settled t ON t.id = s.trade_id
    WHERE s.event = 'paid';
END $$;

COMMIT;
