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
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS positive_slippage_exempt BOOLEAN;
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
END $$;

DO $$
BEGIN
    IF to_regclass('public.subsidies') IS NULL OR to_regclass('public.trades') IS NULL THEN
        RAISE NOTICE 'subsidies or trades does not exist yet, skipping the subsidy view';
        RETURN;
    END IF;

    -- Each subsidy the payer paid, next to the settled trade it was paid on: what the swap was
    -- worth and what the subsidy cost.
    --
    -- `subsidy_usd` is known when the subsidy was paid in the trade's output token, which is
    -- the price the pricer has. A subsidy on an earlier hop is paid in that hop's token and
    -- stays unpriced here.
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
