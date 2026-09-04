-- The tokens trusted to price a trade, pinned by address.
--
-- Two jobs:
--   * the rows with is_stable carry the USD anchor of their chain. Tycho prices every token in
--     the chain's native token, so `price / 10^decimals` of a stablecoin is the USD price of the
--     native token. The median over several stables survives one thin or stale row.
--   * any row makes its side of a trade usable for pricing, which is what lets the long tail be
--     valued at all: only one side of a trade has to be a token we trust.
--
-- Pinned by address, never by symbol. Tycho holds many tokens with a copied symbol -- base has
-- about 40 "cbBTC" rows, ethereum has a second "USDC" with 18 decimals -- and their prices are
-- nonsense. `decimals` is also kept here rather than read from Tycho, whose token rows carry 18
-- for anything it has not analysed. min_usd/max_usd bound the resulting unit price and drop a row
-- that has drifted or was never real.
--
--   psql "$DSN" -f pricing/preferred_tokens.sql
BEGIN;

CREATE TABLE IF NOT EXISTS preferred_tokens (
    chain     TEXT             NOT NULL,
    address   TEXT             NOT NULL,
    symbol    TEXT             NOT NULL,
    decimals  INTEGER          NOT NULL,
    -- A stablecoin, worth 1 USD. Anchors the chain's native price and, because that value does
    -- not move, prices a trade of any age.
    is_stable BOOLEAN          NOT NULL,
    -- Lower wins when both sides of a trade are preferred.
    priority  INTEGER          NOT NULL,
    min_usd   DOUBLE PRECISION NOT NULL,
    max_usd   DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (chain, address)
);

TRUNCATE preferred_tokens;

-- Native sentinel used by the router calldata for every chain, priced as 1 native token.
INSERT INTO preferred_tokens (chain, address, symbol, decimals, is_stable, priority, min_usd, max_usd)
SELECT c.chain, '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', c.symbol, 18, false, 2, c.min_usd, c.max_usd
FROM (VALUES
    ('ethereum',  'ETH', 100.0, 100000.0),
    ('base',      'ETH', 100.0, 100000.0),
    ('unichain',  'ETH', 100.0, 100000.0),
    ('arbitrum',  'ETH', 100.0, 100000.0),
    ('robinhood', 'ETH', 100.0, 100000.0),
    ('bsc',       'BNB',  20.0,  10000.0),
    ('polygon',   'POL',  0.005,   100.0)
) AS c(chain, symbol, min_usd, max_usd);

INSERT INTO preferred_tokens (chain, address, symbol, decimals, is_stable, priority, min_usd, max_usd)
VALUES
    -- ethereum
    ('ethereum', '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'USDC',  6, true,  1, 0.9, 1.1),
    ('ethereum', '0xdac17f958d2ee523a2206206994597c13d831ec7', 'USDT',  6, true,  1, 0.9, 1.1),
    ('ethereum', '0x6b175474e89094c44da98b954eedeac495271d0f', 'DAI',  18, true,  1, 0.9, 1.1),
    ('ethereum', '0xe343167631d89b6ffc58b88d6b7fb0228795491d', 'USDG',  6, true,  1, 0.9, 1.1),
    ('ethereum', '0x4c9edd5852cd905f086c759e8383e09bff1e68b3', 'USDe', 18, false, 2, 0.5, 2.0),
    ('ethereum', '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2', 'WETH', 18, false, 2, 100.0, 100000.0),
    ('ethereum', '0x2260fac5e5542a773aa44fbcfedf7c193bc2c599', 'WBTC',  8, false, 3, 1000.0, 1000000.0),
    ('ethereum', '0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf', 'cbBTC', 8, false, 3, 1000.0, 1000000.0),
    -- base
    ('base', '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913', 'USDC',  6, true,  1, 0.9, 1.1),
    ('base', '0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca', 'USDbC', 6, true,  1, 0.9, 1.1),
    ('base', '0xfde4c96c8593536e31f229ea8f37b2ada2699bb2', 'USDT',  6, true,  1, 0.9, 1.1),
    ('base', '0x4200000000000000000000000000000000000006', 'WETH', 18, false, 2, 100.0, 100000.0),
    ('base', '0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf', 'cbBTC', 8, false, 3, 1000.0, 1000000.0),
    ('base', '0x0555e30da8f98308edb960aa94c0db47230d2b9c', 'WBTC',  8, false, 3, 1000.0, 1000000.0),
    -- unichain
    ('unichain', '0x078d782b760474a361dda0af3839290b0ef57ad6', 'USDC',  6, true,  1, 0.9, 1.1),
    ('unichain', '0x9151434b16b9763660705744891fa906f660ecc5', 'USDT0', 6, true,  1, 0.9, 1.1),
    ('unichain', '0x4200000000000000000000000000000000000006', 'WETH', 18, false, 2, 100.0, 100000.0),
    -- arbitrum
    ('arbitrum', '0xaf88d065e77c8cc2239327c5edb3a432268e5831', 'USDC',   6, true,  1, 0.9, 1.1),
    ('arbitrum', '0xff970a61a04b1ca14834a43f5de4533ebddb5cc8', 'USDC.e', 6, true,  1, 0.9, 1.1),
    ('arbitrum', '0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9', 'USDT0',  6, true,  1, 0.9, 1.1),
    ('arbitrum', '0xda10009cbd5d07dd0cecc66161fc93d7c9000da1', 'DAI',   18, true,  1, 0.9, 1.1),
    ('arbitrum', '0x82af49447d8a07e3bd95bd0d56f35241523fbab1', 'WETH',  18, false, 2, 100.0, 100000.0),
    ('arbitrum', '0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f', 'WBTC',   8, false, 3, 1000.0, 1000000.0),
    ('arbitrum', '0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf', 'cbBTC',  8, false, 3, 1000.0, 1000000.0),
    -- bsc: prices are in BNB
    ('bsc', '0x55d398326f99059ff775485246999027b3197955', 'USDT',  18, true,  1, 0.9, 1.1),
    ('bsc', '0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d', 'USDC',  18, true,  1, 0.9, 1.1),
    ('bsc', '0xc5f0f7b66764f6ec8c8dff7ba683102295e16409', 'FDUSD', 18, true,  1, 0.9, 1.1),
    ('bsc', '0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3', 'DAI',   18, true,  1, 0.9, 1.1),
    ('bsc', '0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c', 'WBNB',  18, false, 2, 20.0, 10000.0),
    ('bsc', '0x2170ed0880ac9a755fd29b2688956bd959f933f8', 'ETH',   18, false, 2, 100.0, 100000.0),
    ('bsc', '0x7130d2a12b9bcbfae4f2634d864a1ee1ce3ead9c', 'BTCB',  18, false, 3, 1000.0, 1000000.0),
    -- polygon: prices are in POL
    ('polygon', '0x3c499c542cef5e3811e1192ce70d8cc03d5c3359', 'USDC',   6, true,  1, 0.9, 1.1),
    ('polygon', '0x2791bca1f2de4661ed88a30c99a7a9449aa84174', 'USDC.e', 6, true,  1, 0.9, 1.1),
    ('polygon', '0xc2132d05d31c914a87c6611c10748aeb04b58e8f', 'USDT0',  6, true,  1, 0.9, 1.1),
    ('polygon', '0x8f3cf7ad23cd3cadbd9735aff958023239c6a063', 'DAI',   18, true,  1, 0.9, 1.1),
    ('polygon', '0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270', 'WPOL',  18, false, 2, 0.005, 100.0),
    ('polygon', '0x7ceb23fd6bc0add59e62ac25578270cff1b9f619', 'WETH',  18, false, 2, 100.0, 100000.0),
    ('polygon', '0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6', 'WBTC',   8, false, 3, 1000.0, 1000000.0),
    -- robinhood: native is ETH, and USDG is the stable that trades there
    ('robinhood', '0x5fc5360d0400a0fd4f2af552add042d716f1d168', 'USDG',  6, true,  1, 0.9, 1.1),
    ('robinhood', '0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34', 'USDe', 18, false, 2, 0.5, 2.0),
    ('robinhood', '0x0bd7d308f8e1639fab988df18a8011f41eacad73', 'WETH', 18, false, 2, 100.0, 100000.0),
    ('robinhood', '0xcec185eb182c47d1ba1efc84e6959e18cd620be4', 'cbBTC', 8, false, 3, 1000.0, 1000000.0);

COMMIT;
