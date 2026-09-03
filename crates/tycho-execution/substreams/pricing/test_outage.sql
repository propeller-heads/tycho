INSERT INTO trades (
    id, chain, block_number, block_time, tx_hash, tx_index, call_index,
    tx_success, call_success, router, router_version, strategy, funding,
    eoa, msg_sender, receiver, token_in, token_out, amount_in, min_amount_out,
    native_value, gas_used, n_tokens, n_hops, executors, protocol_systems, wrap_eth, unwrap_eth
) VALUES (
    'outage-probe', 'ethereum', 2, now(), '0x08', 0, 0,
    true, true, '0x01', 'v3_1', 'single', 'transfer_from',
    '0x01', '0x01', '0x01', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    '0xdddddddddddddddddddddddddddddddddddddddd', 1000000000000000000, 0,
    0, 1, 0, 1, '{}', '{}', false, false
);
