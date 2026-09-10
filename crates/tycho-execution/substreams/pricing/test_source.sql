CREATE TABLE IF NOT EXISTS account (
    id BIGINT PRIMARY KEY,
    address BYTEA NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS token (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL UNIQUE REFERENCES account(id),
    decimals INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS token_price (
    token_id BIGINT PRIMARY KEY REFERENCES token(id),
    price DOUBLE PRECISION NOT NULL,
    modified_ts TIMESTAMPTZ NOT NULL
);

INSERT INTO account (id, address)
VALUES (:id, decode(:'token_hex', 'hex'));
INSERT INTO token (id, account_id, decimals)
VALUES (:id, :id, :decimals);
INSERT INTO token_price (token_id, price, modified_ts)
VALUES (:id, :price, now() - :'age'::interval);
