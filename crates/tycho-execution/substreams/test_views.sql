-- Fixtures and assertions for the views in views.sql.
--
-- Run through scripts/test_views.sh, which applies schema.sql and views.sql first.
BEGIN;

-- A minimal trade. Only the columns the view reads carry meaning.
CREATE OR REPLACE FUNCTION add_trade(
    p_id TEXT, p_tx TEXT, p_call_index INTEGER, p_in TEXT, p_out TEXT, p_amount_in NUMERIC,
    p_tx_success BOOLEAN DEFAULT TRUE, p_call_success BOOLEAN DEFAULT TRUE,
    p_state_committed BOOLEAN DEFAULT NULL
) RETURNS VOID AS $$
    INSERT INTO trades (
        id, chain, block_number, block_time, tx_hash, tx_index, call_index, tx_success,
        call_success, router, router_version, strategy, funding, eoa, msg_sender, receiver,
        token_in, token_out, amount_in, min_amount_out, native_value, gas_used, n_tokens, n_hops,
        executors, wrap_eth, unwrap_eth, state_committed
    ) VALUES (
        p_id, 'robinhood', 1, '2026-09-01T00:00:00Z', p_tx, 0, p_call_index, p_tx_success,
        p_call_success, '0xr', 'v3_1', 'sequential', 'transfer_from', '0xe', '0xm', '0xr2',
        p_in, p_out, p_amount_in, 0, 0, 0, 2, 1, '{}', FALSE, FALSE, p_state_committed
    );
$$ LANGUAGE sql;

DO $$
BEGIN
    -- A call the chain kept, and one it threw away.
    PERFORM add_trade('committed', '0xaa', 10, '0xin', '0xout', 100, TRUE, TRUE, TRUE);
    PERFORM add_trade('discarded', '0xbb', 10, '0xin', '0xout', 100, TRUE, TRUE, FALSE);

    -- A probe and the fill its transaction went on to execute. Only the fill is kept, and the
    -- flag is what says so: the probe has the lower call index, not the higher one.
    PERFORM add_trade('probe', '0xcc', 96,  '0xin', '0xout', 100, TRUE, TRUE, FALSE);
    PERFORM add_trade('fill',  '0xcc', 201, '0xin', '0xout', 100, TRUE, TRUE, TRUE);

    -- A fill followed by a probe of the same swap, so the probe is the later call.
    PERFORM add_trade('fill-first',  '0xdd', 10, '0xin', '0xout', 100, TRUE, TRUE, TRUE);
    PERFORM add_trade('probe-after', '0xdd', 20, '0xin', '0xout', 100, TRUE, TRUE, FALSE);

    -- The same swap twice in one transaction, both kept by the chain. Both are trades: a route
    -- can legitimately hit one pair twice with the same input amount.
    PERFORM add_trade('twin-a', '0xee', 10, '0xin', '0xout', 100, TRUE, TRUE, TRUE);
    PERFORM add_trade('twin-b', '0xee', 20, '0xin', '0xout', 100, TRUE, TRUE, TRUE);

    -- A reverted call, and a failed transaction. Neither is a settled trade, and both carry the
    -- flag set here on purpose: a reverted call discards its own state and a failed transaction
    -- discards all of it, so the combination should not arise, but the view does not lean on
    -- that. Its three filters each stand on their own.
    PERFORM add_trade('reverted',  '0xff', 10, '0xin', '0xout', 100, TRUE,  FALSE, TRUE);
    PERFORM add_trade('tx-failed', '0xgg', 10, '0xin', '0xout', 100, FALSE, TRUE,  TRUE);

    -- A row indexed before the flag existed. Nothing in it says the chain kept the call, so it
    -- stays out until the chain is re-indexed.
    PERFORM add_trade('legacy', '0xhh', 10, '0xin', '0xout', 100, TRUE, TRUE, NULL);
END $$;

DO $$
DECLARE
    kept TEXT[];
BEGIN
    SELECT array_agg(id ORDER BY id) INTO kept FROM trades_settled;
    IF kept IS DISTINCT FROM ARRAY['committed', 'fill', 'fill-first', 'twin-a', 'twin-b'] THEN
        RAISE EXCEPTION 'trades_settled kept %', kept;
    END IF;

    -- The view carries every column of trades, so a caller can read volume straight off it.
    IF (SELECT count(*) FROM information_schema.columns WHERE table_name = 'trades_settled')
       IS DISTINCT FROM (SELECT count(*) FROM information_schema.columns WHERE table_name = 'trades')
    THEN
        RAISE EXCEPTION 'trades_settled does not carry every column of trades';
    END IF;
END $$;

-- Vault balances come from two sources, so the fixtures below feed both: `fees_taken` for the
-- credit the router mints without an event, `vault_transfers` for everything that logs one.
CREATE OR REPLACE FUNCTION add_fee(
    p_trade TEXT, p_token TEXT, p_recipient TEXT, p_amount NUMERIC, p_role TEXT DEFAULT 'router'
) RETURNS VOID AS $$
    INSERT INTO fees_taken (id, trade_id, chain, block_number, token, recipient, amount, role)
    VALUES (p_trade || ':' || p_recipient || ':' || p_token, p_trade, 'robinhood', 1,
            p_token, p_recipient, p_amount, p_role);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION add_vault(
    p_id TEXT, p_sender TEXT, p_receiver TEXT, p_token TEXT, p_amount NUMERIC, p_kind TEXT
) RETURNS VOID AS $$
    INSERT INTO vault_transfers (
        id, chain, block_number, block_time, tx_hash, log_index, router, caller, sender,
        receiver, token, amount, kind
    ) VALUES (
        p_id, 'robinhood', 2, '2026-09-01T00:00:00Z', '0x' || p_id, 0, '0xr', '0xcaller',
        p_sender, p_receiver, p_token, p_amount, p_kind
    );
$$ LANGUAGE sql;

DO $$
DECLARE
    zero TEXT := '0x0000000000000000000000000000000000000000';
BEGIN
    -- Fees on two calls the chain kept.
    PERFORM add_fee('committed', '0xusdc', '0xfeetaker', 300);
    PERFORM add_fee('fill', '0xusdc', '0xfeetaker', 200);

    -- A fee on a call the chain threw away. The credit never happened on chain.
    PERFORM add_fee('discarded', '0xusdc', '0xfeetaker', 999);
    -- A fee on a reverted call and on a failed transaction. Neither reached the vault, and both
    -- of those trades carry the flag, so only tx_success and call_success keep them out.
    PERFORM add_fee('reverted', '0xusdc', '0xfeetaker', 888);
    PERFORM add_fee('tx-failed', '0xusdc', '0xfeetaker', 777);

    -- A fee on a row indexed before the flag existed. Nothing says the chain kept the call.
    PERFORM add_fee('legacy', '0xusdc', '0xfeetaker', 666);

    -- A client on zero bps. The row exists but moved nothing, so no owner appears for it.
    PERFORM add_fee('committed', '0xusdc', '0xclient', 0, 'client');

    -- The fee taker withdraws 120 of the 500 it holds.
    PERFORM add_vault('w1', '0xfeetaker', zero, '0xusdc', 120, 'debit');

    -- Someone deposits, moves it on, and the second owner takes it all out again.
    PERFORM add_vault('d1', zero, '0xalice', '0xweth', 70, 'credit');
    PERFORM add_vault('t1', '0xalice', '0xbob', '0xweth', 70, 'transfer');
    PERFORM add_vault('w2', '0xbob', zero, '0xweth', 70, 'debit');
END $$;

DO $$
DECLARE
    balance NUMERIC;
    fees NUMERIC;
    owners TEXT[];
BEGIN
    -- 300 + 200 credited by fees, 120 withdrawn. The discarded, reverted and failed fees are out.
    SELECT b.balance, b.credited_as_fees INTO balance, fees
    FROM vault_balances b
    WHERE b.owner = '0xfeetaker' AND b.token = '0xusdc';
    IF balance IS DISTINCT FROM 380 OR fees IS DISTINCT FROM 500 THEN
        RAISE EXCEPTION 'fee taker holds % of % credited, expected 380 of 500', balance, fees;
    END IF;

    -- A zero-amount fee row puts nobody in the vault.
    IF EXISTS (SELECT 1 FROM vault_balances b WHERE b.owner = '0xclient') THEN
        RAISE EXCEPTION 'a zero fee created a vault owner';
    END IF;

    -- The deposit reached alice, the move took it away again, and bob withdrew it. Both stay
    -- listed on zero so the flow through them is still visible.
    SELECT array_agg(b.owner || '=' || b.balance ORDER BY b.owner) INTO owners
    FROM vault_balances b WHERE b.token = '0xweth';
    IF owners IS DISTINCT FROM ARRAY['0xalice=0', '0xbob=0'] THEN
        RAISE EXCEPTION 'weth balances are %', owners;
    END IF;

    -- The zero address is a mint and burn marker, not an owner.
    IF EXISTS (
        SELECT 1 FROM vault_balances b
        WHERE b.owner = '0x0000000000000000000000000000000000000000'
    ) THEN
        RAISE EXCEPTION 'the zero address became a vault owner';
    END IF;

    -- Nothing here withdraws more than it was credited, so no balance may go negative. A
    -- negative one on real data means a credit source is missing.
    IF EXISTS (SELECT 1 FROM vault_balances b WHERE b.balance < 0) THEN
        RAISE EXCEPTION 'a balance went negative';
    END IF;
END $$;

DROP FUNCTION add_fee(TEXT, TEXT, TEXT, NUMERIC, TEXT);
DROP FUNCTION add_vault(TEXT, TEXT, TEXT, TEXT, NUMERIC, TEXT);
DROP FUNCTION add_trade(TEXT, TEXT, INTEGER, TEXT, TEXT, NUMERIC, BOOLEAN, BOOLEAN, BOOLEAN);
COMMIT;
