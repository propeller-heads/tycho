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
-- Implying it needs only that side's decimals, which are token metadata: they come from Tycho's
-- token table (or the pinned list) whether or not the token has a fresh price.
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

-- The tokens this pass needs from Tycho: both sides of every unpriced trade, and the chain's
-- pinned tokens (the anchor). Passed as a literal so the lookup below runs on the Tycho database
-- instead of copying its whole token table across the link on every pass.
SELECT COALESCE(array_agg(DISTINCT decode(substr(token, 3), 'hex')), '{}')::text AS needed_tokens
FROM (
    SELECT token_in AS token FROM trades
    WHERE priced_at IS NULL AND chain = :'chain' AND tx_success AND call_success
    UNION ALL
    SELECT token_out FROM trades
    WHERE priced_at IS NULL AND chain = :'chain' AND tx_success AND call_success
    UNION ALL
    SELECT address FROM preferred_tokens WHERE chain = :'chain'
) needed
WHERE token ~ '^0x[0-9a-f]{40}$'
\gset

-- Every needed token Tycho knows, with its decimals whether or not it is priced, and its price
-- only while fresh. Decimals are token metadata, so a token without a fresh price can still have
-- the price a trade implies for it.
WITH tycho_tokens AS (
    SELECT '0x' || encode(a.address, 'hex')                        AS token,
           CASE WHEN tp.price > 0
                     AND tp.modified_ts > now() - :'price_max_age'::interval
                THEN tp.price::numeric END                          AS price_raw,
           -- Tycho reads decimals() when it first sees a token and stores 18 if that call fails.
           -- A failed call usually fails the token's analysis too (quality 10 or below), so an
           -- 18 on such a token is not trusted.
           CASE WHEN t.decimals = 18 AND t.quality <= 10 THEN NULL
                ELSE t.decimals END                                 AS tycho_decimals
    FROM :schema.token t
    JOIN :schema.account a ON a.id = t.account_id
    LEFT JOIN :schema.token_price tp ON tp.token_id = t.id
    -- A bare column against a constant array, so postgres_fdw sends the filter to Tycho.
    WHERE a.address = ANY (:'needed_tokens'::bytea[])
),
-- The router spends and receives the native token under this sentinel. One native token is
-- 10^18 of its own raw units, which is what a Tycho price means.
tokens AS (
    SELECT * FROM tycho_tokens
    UNION ALL
    SELECT '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', power(10::numeric, 18), 18
),
anchor AS (
    SELECT percentile_cont(0.5) WITHIN GROUP (
               ORDER BY k.price_raw / power(10::numeric, pt.decimals)
           ) AS native_usd
    FROM preferred_tokens pt
    JOIN tokens k ON k.token = pt.address
    WHERE pt.chain = :'chain'
      AND pt.is_stable
      AND k.price_raw IS NOT NULL
),
-- One row per known token. A pinned token's decimals win over Tycho's. `price_raw` is kept only
-- when the token can value a trade: fresh, and for a pinned token inside its band (outside it,
-- it is a copied symbol or a stale row, not a price). Its decimals stay usable either way.
valued AS (
    SELECT k.token,
           CASE WHEN pt.address IS NULL
                     OR (power(10::numeric, pt.decimals) / k.price_raw * a.native_usd)
                        BETWEEN pt.min_usd AND pt.max_usd
                THEN k.price_raw END                    AS price_raw,
           COALESCE(pt.decimals, k.tycho_decimals)      AS decimals,
           (pt.address IS NOT NULL)                     AS preferred,
           COALESCE(pt.is_stable, false)                AS is_stable,
           COALESCE(pt.priority, 99)                    AS priority,
           a.native_usd
    FROM tokens k
    CROSS JOIN anchor a
    LEFT JOIN preferred_tokens pt
           ON pt.chain = :'chain' AND pt.address = k.token
    WHERE a.native_usd IS NOT NULL
),
priced AS (
    SELECT v.*,
           power(10::numeric, v.decimals) / v.price_raw * v.native_usd AS unit_usd,
           (v.preferred AND v.price_raw IS NOT NULL)                    AS can_anchor
    FROM valued v
),
sided AS (
    SELECT t.id,
           t.block_time,
           t.amount_in,
           t.amount_out,
           i.unit_usd AS in_unit, i.price_raw AS in_price_raw,
           -- A token Tycho does not know keeps the decimals the trade already has.
           CASE WHEN i.token IS NULL THEN t.decimals_in ELSE i.decimals END AS in_decimals,
           COALESCE(i.can_anchor, false) AS in_preferred, i.is_stable AS in_stable,
           i.priority AS in_priority,
           o.unit_usd AS out_unit, o.price_raw AS out_price_raw,
           CASE WHEN o.token IS NULL THEN t.decimals_out ELSE o.decimals END AS out_decimals,
           COALESCE(o.can_anchor, false) AS out_preferred, o.is_stable AS out_stable,
           o.priority AS out_priority,
           COALESCE(i.native_usd, o.native_usd) AS native_usd
    FROM trades t
    LEFT JOIN priced i ON i.token = t.token_in
    LEFT JOIN priced o ON o.token = t.token_out
    WHERE t.priced_at IS NULL
      AND t.chain = :'chain'
      AND t.tx_success
      AND t.call_success
      AND (i.price_raw IS NOT NULL OR o.price_raw IS NOT NULL)
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
           -- The side that valued the trade keeps its own unit price. The other side is always
           -- implied from this trade, never taken from Tycho, for two reasons: Tycho holds one
           -- current price and this trade may be a year old, and an implied price keeps the row
           -- consistent, so price x amount equals volume_usd on both sides.
           (CASE
               WHEN c.side = 'in' THEN c.in_unit
               ELSE c.volume_usd / (NULLIF(c.amount_in, 0) / power(10::numeric, c.in_decimals))
           END)::double precision AS price_in_usd,
           (CASE
               WHEN c.side = 'out' THEN c.out_unit
               ELSE c.volume_usd / (NULLIF(c.amount_out, 0) / power(10::numeric, c.out_decimals))
           END)::double precision AS price_out_usd,
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
