-- Exposes the price tables of one chain's Tycho indexer database in this database through
-- postgres_fdw, in the schema `tycho_<chain>`. Run once per chain (each chain has its own Tycho
-- database). Pricing queries each resulting schema independently.
--
--   psql "$DSN" -v chain=ethereum -v tycho_host=... -v tycho_port=5432 -v tycho_db=... \
--        -v tycho_user=... -v tycho_password=... -f pricing/tycho_foreign_tables.sql
CREATE EXTENSION IF NOT EXISTS postgres_fdw;

\set server 'tycho_' :chain
\set schema 'tycho_' :chain

DROP SERVER IF EXISTS :server CASCADE;
CREATE SERVER :server
    FOREIGN DATA WRAPPER postgres_fdw
    OPTIONS (host :'tycho_host', port :'tycho_port', dbname :'tycho_db', fetch_size '10000');

CREATE USER MAPPING FOR CURRENT_USER
    SERVER :server
    OPTIONS (user :'tycho_user', password :'tycho_password');

DROP SCHEMA IF EXISTS :schema CASCADE;
CREATE SCHEMA :schema;
IMPORT FOREIGN SCHEMA public LIMIT TO (token_price, token, account)
    FROM SERVER :server INTO :schema;
