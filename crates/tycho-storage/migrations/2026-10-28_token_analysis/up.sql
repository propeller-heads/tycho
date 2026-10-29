-- Outcome of the latest analysis of each token. The scalars live on "token"
-- so they can be filtered on; the details live in "token_analysis", which token reads do not
-- load. Both are replaced on every re-analysis.
ALTER TABLE "token"
    -- ok, fee, temporarily_blocked, blocked, no_quote or no_liquidity; NULL if never analysed
    -- by an analyzer that reports a verdict.
    ADD COLUMN "verdict" varchar(32) DEFAULT NULL,
    -- Fee per leg in basis points of the amount sent; NULL when the leg did not run.
    ADD COLUMN "buy_tax" bigint DEFAULT NULL,
    ADD COLUMN "transfer_tax" bigint DEFAULT NULL,
    ADD COLUMN "sell_tax" bigint DEFAULT NULL,
    -- Block the analysis ran at.
    ADD COLUMN "analyzed_block" bigint DEFAULT NULL;

CREATE INDEX IF NOT EXISTS idx_token_verdict ON "token"(verdict);

CREATE TABLE IF NOT EXISTS token_analysis(
    "token_id" bigint PRIMARY KEY REFERENCES "token"(id) ON DELETE CASCADE,
    -- Analyzer name and version, e.g. simulation/1 or ethcall/1.
    "analyzer" varchar(64) NOT NULL,
    -- Why the verdict is not ok, e.g. the token's revert reason.
    "reason" text,
    -- The address the simulated buy came from and the sell went to.
    "pool" bytea,
    -- Average gas of a transfer.
    "gas" bigint,
    -- Behaviour that did not decide the verdict on its own, e.g. ["balance_writer"].
    "flags" jsonb NOT NULL,
    -- Calls the token made during the simulated legs.
    "external_calls" jsonb NOT NULL,
    -- Token storage fields the transfer path reads and an owner-callable function writes.
    "control_slots" jsonb NOT NULL,
    "inserted_ts" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "modified_ts" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TRIGGER update_modtime_token_analysis
    BEFORE UPDATE ON "token_analysis"
    FOR EACH ROW
    EXECUTE PROCEDURE update_modified_column();
