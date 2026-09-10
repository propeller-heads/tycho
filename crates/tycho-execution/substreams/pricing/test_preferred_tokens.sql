-- Test fixture: the synthetic tokens the pricing test trusts. Applied after
-- pricing/preferred_tokens.sql, whose real addresses have no price rows in the test sources and
-- are therefore ignored.
INSERT INTO preferred_tokens (chain, address, symbol, decimals, is_stable, priority, min_usd, max_usd)
VALUES
    -- ethereum source: native is worth 2000 USD through this stable
    ('ethereum', '0xffffffffffffffffffffffffffffffffffffffff', 'TUSD', 6, true,  1, 0.9, 1.1),
    ('ethereum', '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'TWETH', 18, false, 2, 100.0, 100000.0),
    -- base source: native is worth 1000 USD, a different anchor on purpose
    ('base', '0xfff1111111111111111111111111111111111111', 'TUSD', 6, true,  1, 0.9, 1.1),
    ('base', '0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'TWETH', 6, false, 2, 100.0, 100000.0),
    -- a pinned token whose price puts it outside its band: it must be ignored, not trusted
    ('base', '0xdeadbeef00000000000000000000000000000000', 'FAKE', 18, false, 3, 1000.0, 1000000.0);
