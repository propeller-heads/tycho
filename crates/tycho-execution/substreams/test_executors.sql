-- Fixtures and assertions for executors.sql and the views it creates.
--
-- Run through scripts/test_executors.sh, which applies schema.sql and executors.sql first.
BEGIN;

-- The same address on two chains, with different names, to check which row a hop picks.
INSERT INTO executors (chain, address, protocol_systems) VALUES
    ('ethereum', '0xabcabcabcabcabcabcabcabcabcabcabcabcabca', ARRAY['ethereum_only']),
    ('base',     '0xabcabcabcabcabcabcabcabcabcabcabcabcabca', ARRAY['base_only']);

INSERT INTO trade_hops (id, trade_id, chain, block_number, hop_index, executor, protocol_data)
VALUES
    -- Two hops of one trade through two known ethereum executors.
    ('t1:0', 't1', 'ethereum', 1, 0, '0x0017c84f2b3414514b67bfc9a63830c8e0e690d0', '0x'),
    ('t1:1', 't1', 'ethereum', 1, 1, '0xfee95e97db5fdfcde672b9a06f4be87032dd7689', '0x'),
    -- An address the docs list under ethereum only, used on base.
    ('t2:0', 't2', 'base', 2, 0, '0x0017c84f2b3414514b67bfc9a63830c8e0e690d0', '0x'),
    -- An executor no table knows.
    ('t3:0', 't3', 'ethereum', 3, 0, '0xdeadbeef00000000000000000000000000000000', '0x'),
    -- One known and one unknown executor in the same trade.
    ('t4:0', 't4', 'ethereum', 4, 0, '0xfee95e97db5fdfcde672b9a06f4be87032dd7689', '0x'),
    ('t4:1', 't4', 'ethereum', 4, 1, '0xdeadbeef00000000000000000000000000000000', '0x'),
    -- An address with a row on the hop's own chain and on another one.
    ('t5:0', 't5', 'base', 5, 0, '0xabcabcabcabcabcabcabcabcabcabcabcabcabca', '0x');

DO $$
DECLARE
    v RECORD;
BEGIN
    -- A known executor carries its names, sorted and deduplicated per trade.
    IF (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't1')
       IS DISTINCT FROM ARRAY['ekubo_v2', 'sushiswap_v2', 'uniswap_v2'] THEN
        RAISE EXCEPTION 'two known hops resolved to %',
            (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't1');
    END IF;

    -- An address with no row for this chain falls back to the chains that do have one.
    IF (SELECT protocol_systems FROM trade_hop_protocols WHERE trade_id = 't2')
       IS DISTINCT FROM ARRAY['sushiswap_v2', 'uniswap_v2'] THEN
        RAISE EXCEPTION 'cross-chain fallback resolved to %',
            (SELECT protocol_systems FROM trade_hop_protocols WHERE trade_id = 't2');
    END IF;

    -- An address with a row for this chain uses it and ignores the other chains.
    IF (SELECT protocol_systems FROM trade_hop_protocols WHERE trade_id = 't5')
       IS DISTINCT FROM ARRAY['base_only'] THEN
        RAISE EXCEPTION 'the hop chain did not win: %',
            (SELECT protocol_systems FROM trade_hop_protocols WHERE trade_id = 't5');
    END IF;

    -- An unknown executor keeps its hop and gets an empty array.
    SELECT * INTO v FROM trade_hop_protocols WHERE trade_id = 't3';
    IF v.executor IS DISTINCT FROM '0xdeadbeef00000000000000000000000000000000'
       OR v.protocol_systems IS DISTINCT FROM ARRAY[]::TEXT[] THEN
        RAISE EXCEPTION 'unknown executor hop wrong: % %', v.executor, v.protocol_systems;
    END IF;
    IF EXISTS (SELECT 1 FROM trade_protocol_systems WHERE trade_id = 't3') THEN
        RAISE EXCEPTION 'a trade with no known executor got a row';
    END IF;

    -- A trade keeps the names it does know.
    IF (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't4')
       IS DISTINCT FROM ARRAY['ekubo_v2'] THEN
        RAISE EXCEPTION 'partly known trade resolved to %',
            (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't4');
    END IF;
END $$;

COMMIT;
