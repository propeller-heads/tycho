INSERT INTO trades (
    id, chain, block_number, block_time, tx_hash, tx_index, call_index,
    tx_success, call_success, router, router_version, strategy, funding,
    eoa, msg_sender, receiver, token_in, token_out, amount_in, min_amount_out,
    native_value, gas_used, n_tokens, n_hops, executors, protocol_systems, wrap_eth, unwrap_eth,
    amount_out
) VALUES
    -- in side is a preferred non-stable worth 4000 USD: 1 token in, unknown token out
    ('in-preferred', 'ethereum', 1, now(), '0x01', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
     '0xdddddddddddddddddddddddddddddddddddddddd', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 4000000),
    -- a stablecoin on the out side prices a trade of any age, here 30 days old
    ('out-stable-old', 'ethereum', 1, now() - interval '30 days', '0x02', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xdddddddddddddddddddddddddddddddddddddddd',
     '0xffffffffffffffffffffffffffffffffffffffff', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 1500000000),
    -- both sides preferred: the lower priority wins, so the stable prices it
    ('both-preferred', 'ethereum', 1, now(), '0x03', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
     '0xffffffffffffffffffffffffffffffffffffffff', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 3000000000),
    -- the native sentinel is priced as one native token
    ('native-in', 'ethereum', 1, now(), '0x04', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee',
     '0xdddddddddddddddddddddddddddddddddddddddd', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, NULL),
    -- no preferred token, but Tycho prices the out side: allowed while the trade is fresh
    ('out-tycho', 'base', 1, now(), '0x05', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xdddddddddddddddddddddddddddddddddddddddd',
     '0xeee1111111111111111111111111111111111111', 1000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 2000000),
    -- base anchors on its own stable, so the same amount is worth half of the ethereum one
    ('base-preferred', 'base', 1, now(), '0x06', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xdddddddddddddddddddddddddddddddddddddddd',
     '0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 4000000),
    -- a preferred token whose price sits outside its band must not price anything
    ('out-of-band', 'base', 1, now(), '0x07', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xdddddddddddddddddddddddddddddddddddddddd',
     '0xdeadbeef00000000000000000000000000000000', 1000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, 1000000000000000000),
    -- ineligible: a non-stable basis on a trade older than the window
    ('old-nonstable', 'ethereum', 1, now() - interval '2 hours', '0x08', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
     '0xdddddddddddddddddddddddddddddddddddddddd', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, NULL),
    -- ineligible: the call reverted
    ('failed-call', 'ethereum', 1, now(), '0x09', 0, 0,
     true, false, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
     '0xdddddddddddddddddddddddddddddddddddddddd', 1000000000000000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, NULL),
    -- ineligible: the only priced side has a stale Tycho price
    ('stale-price', 'base', 1, now(), '0x0a', 0, 0,
     true, true, '0x01', 'v3_1', 'single', 'transfer_from',
     '0x01', '0x01', '0x01', '0xcccccccccccccccccccccccccccccccccccccccc',
     '0xdddddddddddddddddddddddddddddddddddddddd', 1000000, 0,
     0, 1, 0, 1, '{}', '{}', false, false, NULL);
