-- A database holds one chain, so the address is the key of a token. Instance tables refer to
-- tokens by address, and a token no longer needs an account row.
--
-- After this migration the previous indexer release cannot start on the database: it joins
-- tokens to accounts. Roll back by switching to the previous deployment, not with down.sql.

UPDATE public.token t
SET address = a.address
FROM public.account a
WHERE a.id = t.account_id;

-- Fails when two tokens have the same address, for example two chains in one database. Fix the
-- data by hand; do not merge or skip rows here.
ALTER TABLE public.token
    ALTER COLUMN address SET NOT NULL,
    ADD CONSTRAINT token_address_key UNIQUE (address);

-- Drops the foreign key to account, the unique constraint and the index on the column with it.
ALTER TABLE public.token
    DROP COLUMN account_id;
