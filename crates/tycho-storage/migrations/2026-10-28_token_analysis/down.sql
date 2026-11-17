DROP TABLE IF EXISTS token_analysis;
DROP INDEX IF EXISTS idx_token_verdict;
ALTER TABLE "token"
    DROP COLUMN IF EXISTS "verdict",
    DROP COLUMN IF EXISTS "buy_tax",
    DROP COLUMN IF EXISTS "transfer_tax",
    DROP COLUMN IF EXISTS "sell_tax",
    DROP COLUMN IF EXISTS "analyzed_block";
