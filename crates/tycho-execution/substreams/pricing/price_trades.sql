-- Values one chain's unpriced trades in USD.
--
-- Tycho prices every token in the chain's native token, so nothing here is comparable across
-- chains until it passes through the USD anchor: `preferred_tokens` pins a few stablecoins per
-- chain, and the median of `price / 10^decimals` over them is the USD price of the native token.
-- On bsc that is BNB and on polygon POL, which is why the old ETH-denominated columns were wrong
-- for those two chains rather than merely mislabelled.
--
-- A trade is valued from one side only, choosing in this order:
--   1. a side holding a preferred token, lowest `priority` first (stablecoins, then the native
--      token and its wrapper, then BTC wrappers)
--   2. otherwise any side Tycho happens to price
-- The other side's unit price is then implied from the trade itself, which is the only way the
-- long tail gets a price at all: Tycho prices 29 tokens on unichain and 126 on arbitrum.
--
-- Age. Tycho keeps one current price per token, not a history. A stablecoin is worth 1 USD
-- whenever the trade happened, so a stable-anchored trade is valued at any age. Every other basis
-- would stamp today's price on an old trade, so those are limited to trades younger than
-- :max_age. `price_source` records which basis was used; it is the column to filter on before
-- summing volume.
--
--   psql "$DSN" -v chain=ethereum -v max_age='1 hour' -f pricing/price_trades.sql
\if :{?chain}
\else
\echo 'set chain, e.g. -v chain=ethereum'
\quit
\endif
\set schema 'tycho_' :chain
\if :{?max_age}
\else
\set max_age '1 hour'
\endif
\if :{?price_max_age}
\else
\set price_max_age '3 hours'
\endif

WITH tycho_prices AS (
    SELECT '0x' || encode(a.address, 'hex') AS token,
           tp.price::numeric                AS price_raw,
           t.decimals                       AS tycho_decimals
    FROM :schema.token_price tp
    JOIN :schema.token t ON t.id = tp.token_id
    JOIN :schema.account a ON a.id = t.account_id
    WHERE tp.price > 0
      AND tp.modified_ts > now() - :'price_max_age'::interval
),
-- The router spends and receives the native token under this sentinel. One native token is
-- 10^18 of its own raw units, which is what a Tycho price means.
prices AS (
    SELECT * FROM tycho_prices
    UNION ALL
    SELECT '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', power(10::numeric, 18), 18
),
anchor AS (
    SELECT percentile_cont(0.5) WITHIN GROUP (
               ORDER BY p.price_raw / power(10::numeric, pt.decimals)
           ) AS native_usd
    FROM preferred_tokens pt
    JOIN prices p ON p.token = pt.address
    WHERE pt.chain = :'chain'
      AND pt.is_stable
),
valued AS (
    SELECT p.token,
           p.price_raw,
           COALESCE(pt.decimals, p.tycho_decimals)      AS decimals,
           (pt.address IS NOT NULL)                     AS preferred,
           COALESCE(pt.is_stable, false)                AS is_stable,
           COALESCE(pt.priority, 99)                    AS priority,
           power(10::numeric, COALESCE(pt.decimals, p.tycho_decimals))
               / p.price_raw * a.native_usd             AS unit_usd,
           a.native_usd
    FROM prices p
    CROSS JOIN anchor a
    LEFT JOIN preferred_tokens pt
           ON pt.chain = :'chain' AND pt.address = p.token
    WHERE a.native_usd IS NOT NULL
      -- A pinned token outside its band is a copied symbol or a stale row, not a price.
      AND (pt.address IS NULL
           OR (power(10::numeric, pt.decimals) / p.price_raw * a.native_usd)
               BETWEEN pt.min_usd AND pt.max_usd)
),
sided AS (
    SELECT t.id,
           t.block_time,
           t.amount_in,
           t.amount_out,
           i.unit_usd AS in_unit, i.price_raw AS in_price_raw, i.decimals AS in_decimals,
           i.preferred AS in_preferred, i.is_stable AS in_stable, i.priority AS in_priority,
           o.unit_usd AS out_unit, o.price_raw AS out_price_raw, o.decimals AS out_decimals,
           o.preferred AS out_preferred, o.is_stable AS out_stable, o.priority AS out_priority,
           COALESCE(i.native_usd, o.native_usd) AS native_usd
    FROM trades t
    LEFT JOIN valued i ON i.token = t.token_in
    LEFT JOIN valued o ON o.token = t.token_out
    WHERE t.priced_at IS NULL
      AND t.chain = :'chain'
      AND t.tx_success
      AND t.call_success
      AND (i.token IS NOT NULL OR o.token IS NOT NULL)
),
chosen AS (
    SELECT s.*,
           CASE
               WHEN s.out_preferred AND s.amount_out > 0
                    AND (NOT s.in_preferred OR s.amount_in = 0
                         OR s.out_priority <= s.in_priority)          THEN 'out'
               WHEN s.in_preferred AND s.amount_in > 0                THEN 'in'
               WHEN s.out_price_raw IS NOT NULL AND s.amount_out > 0  THEN 'out'
               WHEN s.in_price_raw IS NOT NULL AND s.amount_in > 0    THEN 'in'
           END AS side
    FROM sided s
),
computed AS (
    SELECT c.id,
           c.native_usd,
           c.side,
           CASE c.side
               WHEN 'out' THEN c.amount_out / c.out_price_raw * c.native_usd
               WHEN 'in'  THEN c.amount_in  / c.in_price_raw  * c.native_usd
           END AS volume_usd,
           CASE WHEN c.side = 'out' THEN c.out_stable ELSE c.in_stable END       AS on_stable,
           CASE WHEN c.side = 'out' THEN c.out_preferred ELSE c.in_preferred END AS on_preferred,
           c.amount_in, c.amount_out,
           c.in_unit, c.out_unit, c.in_decimals, c.out_decimals,
           c.block_time
    FROM chosen c
    WHERE c.side IS NOT NULL
),
final AS (
    SELECT c.id,
           c.native_usd::double precision AS native_usd,
           c.volume_usd::double precision AS volume_usd,
           c.in_decimals,
           c.out_decimals,
           -- The valued side keeps its own unit price; the other side is implied by this trade.
           COALESCE(
               c.in_unit,
               c.volume_usd / (NULLIF(c.amount_in, 0) / power(10::numeric, c.in_decimals))
           )::double precision AS price_in_usd,
           COALESCE(
               c.out_unit,
               c.volume_usd / (NULLIF(c.amount_out, 0) / power(10::numeric, c.out_decimals))
           )::double precision AS price_out_usd,
           c.side || '_' || CASE
               WHEN c.on_stable    THEN 'stable'
               WHEN c.on_preferred THEN 'preferred'
               ELSE 'tycho'
           END AS price_source
    FROM computed c
    WHERE c.volume_usd IS NOT NULL
      AND (c.on_stable OR c.block_time > now() - :'max_age'::interval)
)
UPDATE trades t
SET price_in_usd  = final.price_in_usd,
    price_out_usd = final.price_out_usd,
    decimals_in   = final.in_decimals,
    decimals_out  = final.out_decimals,
    volume_usd    = final.volume_usd,
    native_usd    = final.native_usd,
    price_source  = final.price_source,
    priced_at     = now()
FROM final
WHERE t.id = final.id;
