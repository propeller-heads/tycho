-- Offers every priced trade to the pricing logic again.
--
-- Run this by hand after a change to price_trades.sql that alters values already written. It is
-- deliberately not part of the pricer start-up: a restart must not re-price the whole table.
--
--   psql "$DSN" -f pricing/reprice.sql
UPDATE trades SET priced_at = NULL WHERE priced_at IS NOT NULL;
