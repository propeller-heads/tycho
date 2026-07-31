#!/bin/bash
set -e

# Configuration
S3_BUCKET="repo.propellerheads-propellerheads"
S3_REGION="eu-central-1"
TEMP_DIR="/tmp/token_price_import_$$"

usage() {
    cat << EOF
Usage: $0 --chain CHAIN [--db-url URL]

Required:
  --chain CHAIN    Chain name (ethereum, base, unichain)

Optional:
  --db-url URL     PostgreSQL connection URL 
                   (default: postgresql://postgres:mypassword@localhost:5431/tycho_indexer_0)

Example:
  $0 --chain ethereum
  $0 --chain ethereum --db-url postgresql://user:pass@host:5432/dbname
EOF
    exit 1
}

# Defaults
DB_URL="postgresql://postgres:mypassword@localhost:5431/tycho_indexer_0"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --chain) CHAIN="$2"; shift 2 ;;
        --db-url) DB_URL="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "Unknown argument: $1"; usage ;;
    esac
done

if [[ -z "$CHAIN" ]]; then
    echo "Error: --chain is required"
    usage
fi

# Create temp directory
mkdir -p "$TEMP_DIR"
trap "rm -rf $TEMP_DIR" EXIT

# Download latest CSV (no need to find latest - always use /latest/)
CSV_FILE="${TEMP_DIR}/token-prices.csv"
S3_URL="https://s3.${S3_REGION}.amazonaws.com/${S3_BUCKET}"
CSV_URL="${S3_URL}/token-prices/${CHAIN}/latest/token-prices.csv"

echo "Downloading latest token prices for $CHAIN..."
echo "From: $CSV_URL"

if ! curl -sf --retry 5 --retry-delay 2 --retry-all-errors "$CSV_URL" -o "$CSV_FILE"; then
    echo "Error: Failed to download CSV from $CSV_URL"
    exit 1
fi

ROW_COUNT=$(($(wc -l < "$CSV_FILE") - 1))
echo "CSV contains $ROW_COUNT token prices"

# Run all operations in a single psql session
echo "Importing and updating prices..."
RESULT=$(psql "$DB_URL" -t <<EOF
CREATE TEMP TABLE temp_token_prices (
    address TEXT,
    price TEXT
);

\COPY temp_token_prices(address, price) FROM '$CSV_FILE' WITH CSV HEADER

WITH address_bytes AS (
    SELECT 
        decode(substring(address from 3), 'hex') as address_bin,
        price::float8 as price_float
    FROM temp_token_prices
),
token_lookup AS (
    SELECT 
        t.id as token_id,
        ab.price_float as price
    FROM address_bytes ab
    JOIN account a ON a.address = ab.address_bin
    JOIN token t ON t.account_id = a.id
)
INSERT INTO token_price (token_id, price)
SELECT 
    token_id, 
    price
FROM token_lookup
ON CONFLICT (token_id) 
DO UPDATE SET 
    price = EXCLUDED.price
RETURNING token_id;
EOF
)

AFFECTED_ROWS=$(echo "$RESULT" | grep -v '^$' | wc -l)
SKIPPED=$((ROW_COUNT - AFFECTED_ROWS))

echo "Done! Updated/Inserted: $AFFECTED_ROWS, Skipped: $SKIPPED"
echo "Tip: Run update_component_tvl.sh to recalculate TVL"