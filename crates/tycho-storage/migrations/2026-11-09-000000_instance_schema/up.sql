-- Per-instance schema for protocol isolation.
--
-- Each protocol's data lives in an instance. Current state is list-partitioned by instance_id,
-- with one leaf per instance; history is range-partitioned by day on valid_to across all
-- instances. Nothing reads or writes these tables yet: the old tables keep serving until the data
-- migration. Only additive changes touch public, so the running release is unaffected.
--
-- There is no DEFAULT partition anywhere: DETACH ... CONCURRENTLY is refused on a parent that
-- has one, and adding a partition next to a DEFAULT locks the DEFAULT exclusively.
-- Locks on shared tables are held until the migration commits: SHARE ROW EXCLUSIVE on
-- protocol_system for the FK from protocol_instance, and ACCESS EXCLUSIVE on protocol_system and
-- token for the ADD COLUMNs at the end. If a lock isn't granted within the timeout, the migration
-- fails, the indexer stops at startup, and the restarted process tries again, instead of reads and
-- writes on those tables waiting behind the migration.
SET LOCAL lock_timeout = '5s';

CREATE SCHEMA tycho;

CREATE TYPE tycho.instance_status AS ENUM (
    'loading',
    'ready',
    'dropped'
);

-- Catalog

CREATE TABLE tycho.protocol_instance (
    -- Never reused: dropped instances stay as tombstones.
    id int4 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    protocol_system_id int8 NOT NULL REFERENCES public.protocol_system (id),
    label text NOT NULL,
    status tycho.instance_status NOT NULL,
    -- Block N of the dump or migration the instance was built from; NULL when it was synced in
    -- place from its start block.
    base_block_number int8,
    base_block_hash bytea,
    base_block_ts timestamptz,
    -- extractor | migration | import:<uri>
    source text NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (protocol_system_id, label)
);

CREATE TABLE tycho.extraction_state (
    instance_id int4 PRIMARY KEY REFERENCES tycho.protocol_instance (id),
    cursor bytea,
    block_number int8 NOT NULL,
    block_hash bytea NOT NULL,
    block_ts timestamptz NOT NULL,
    attributes jsonb,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Entity ids are scoped to an instance (keys start with instance_id), so one sequence per entity
-- serves all instances. An import moves each sequence past the largest id it brings in.
CREATE SEQUENCE tycho.protocol_component_id_seq AS int8;
CREATE SEQUENCE tycho.account_id_seq AS int8;
CREATE SEQUENCE tycho.entry_point_id_seq AS int8;
CREATE SEQUENCE tycho.entry_point_tracing_params_id_seq AS int8;
CREATE SEQUENCE tycho.entry_point_tracing_result_id_seq AS int8;

-- Components and protocol state

CREATE TABLE tycho.protocol_component (
    instance_id int4 NOT NULL,
    id int8 NOT NULL DEFAULT nextval('tycho.protocol_component_id_seq'),
    external_id varchar(255) NOT NULL,
    -- Name of a public.protocol_type row.
    protocol_type varchar(255) NOT NULL,
    attributes jsonb,
    created_at timestamptz NOT NULL,
    creation_block int8 NOT NULL,
    creation_tx_hash bytea NOT NULL,
    deleted_at timestamptz,
    deletion_tx_hash bytea,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, id),
    UNIQUE (instance_id, external_id)
)
PARTITION BY LIST (instance_id);

ALTER SEQUENCE tycho.protocol_component_id_seq OWNED BY tycho.protocol_component.id;

CREATE TABLE tycho.protocol_component_holds_token (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    token bytea NOT NULL,
    token_index int2 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, token),
    UNIQUE (instance_id, protocol_component_id, token_index)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.protocol_state (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    attribute_name varchar NOT NULL,
    attribute_value bytea NOT NULL,
    previous_value bytea,
    valid_from timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, attribute_name)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.protocol_state_history (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    attribute_name varchar NOT NULL,
    attribute_value bytea NOT NULL,
    previous_value bytea,
    valid_from timestamptz NOT NULL,
    valid_to timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, attribute_name, valid_to, valid_from, tx_index)
)
PARTITION BY RANGE (valid_to);


CREATE TABLE tycho.component_balance (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    token bytea NOT NULL,
    new_balance bytea NOT NULL,
    previous_value bytea NOT NULL,
    balance_float float8 NOT NULL,
    valid_from timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, token)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.component_balance_history (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    token bytea NOT NULL,
    new_balance bytea NOT NULL,
    previous_value bytea NOT NULL,
    balance_float float8 NOT NULL,
    valid_from timestamptz NOT NULL,
    valid_to timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, token, valid_to, valid_from, tx_index)
)
PARTITION BY RANGE (valid_to);


CREATE TABLE tycho.component_tvl (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    tvl float8 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id)
)
PARTITION BY LIST (instance_id);

CREATE INDEX ON tycho.component_tvl (modified_ts);
CREATE INDEX ON tycho.component_tvl (tvl);

-- Contracts

CREATE TABLE tycho.account (
    instance_id int4 NOT NULL,
    id int8 NOT NULL DEFAULT nextval('tycho.account_id_seq'),
    address bytea NOT NULL,
    title varchar(255) NOT NULL,
    created_at timestamptz,
    creation_block int8,
    creation_tx_hash bytea,
    deleted_at timestamptz,
    deletion_tx_hash bytea,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, id),
    UNIQUE (instance_id, address)
)
PARTITION BY LIST (instance_id);

ALTER SEQUENCE tycho.account_id_seq OWNED BY tycho.account.id;

CREATE TABLE tycho.protocol_component_holds_contract (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    account_id int8 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, protocol_component_id, account_id)
)
PARTITION BY LIST (instance_id);

CREATE INDEX ON tycho.protocol_component_holds_contract (instance_id, account_id);

-- Code rarely changes: the writer adds a version only when the hash changes.
CREATE TABLE tycho.contract_code (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    code bytea NOT NULL,
    hash bytea NOT NULL,
    modify_tx_hash bytea NOT NULL,
    valid_from timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.contract_code_history (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    code bytea NOT NULL,
    hash bytea NOT NULL,
    modify_tx_hash bytea NOT NULL,
    valid_from timestamptz NOT NULL,
    valid_to timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id, valid_to, valid_from, tx_index)
)
PARTITION BY RANGE (valid_to);


CREATE TABLE tycho.contract_storage (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    slot bytea NOT NULL,
    value bytea,
    previous_value bytea,
    valid_from timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id, slot)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.contract_storage_history (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    slot bytea NOT NULL,
    value bytea,
    previous_value bytea,
    valid_from timestamptz NOT NULL,
    valid_to timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id, slot, valid_to, valid_from, tx_index)
)
PARTITION BY RANGE (valid_to);


CREATE TABLE tycho.account_balance (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    token bytea NOT NULL,
    balance bytea NOT NULL,
    valid_from timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id, token)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.account_balance_history (
    instance_id int4 NOT NULL,
    account_id int8 NOT NULL,
    token bytea NOT NULL,
    balance bytea NOT NULL,
    valid_from timestamptz NOT NULL,
    valid_to timestamptz NOT NULL,
    tx_index int4 NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, account_id, token, valid_to, valid_from, tx_index)
)
PARTITION BY RANGE (valid_to);


-- Entry points (DCI)

CREATE TABLE tycho.entry_point (
    instance_id int4 NOT NULL,
    id int8 NOT NULL DEFAULT nextval('tycho.entry_point_id_seq'),
    external_id text NOT NULL,
    target bytea NOT NULL,
    signature text NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, id),
    UNIQUE (instance_id, external_id, target, signature)
)
PARTITION BY LIST (instance_id);

ALTER SEQUENCE tycho.entry_point_id_seq OWNED BY tycho.entry_point.id;

CREATE INDEX ON tycho.entry_point (instance_id, target, signature);

CREATE TABLE tycho.entry_point_tracing_params (
    instance_id int4 NOT NULL,
    id int8 NOT NULL DEFAULT nextval('tycho.entry_point_tracing_params_id_seq'),
    entry_point_id int8 NOT NULL,
    tracing_type public.entry_point_tracing_type NOT NULL,
    data jsonb,
    -- Set by trigger; unique instead of data, whose btree entries can exceed the index row limit.
    data_hash bytea,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, id),
    UNIQUE (instance_id, entry_point_id, tracing_type, data_hash)
)
PARTITION BY LIST (instance_id);

ALTER SEQUENCE tycho.entry_point_tracing_params_id_seq OWNED BY tycho.entry_point_tracing_params.id;

CREATE TABLE tycho.entry_point_tracing_params_calls_account (
    instance_id int4 NOT NULL,
    entry_point_tracing_params_id int8 NOT NULL,
    account_id int8 NOT NULL,
    PRIMARY KEY (instance_id, entry_point_tracing_params_id, account_id)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.entry_point_tracing_result (
    instance_id int4 NOT NULL,
    id int8 NOT NULL DEFAULT nextval('tycho.entry_point_tracing_result_id_seq'),
    entry_point_tracing_params_id int8 NOT NULL,
    detection_block_number int8 NOT NULL,
    detection_block_hash bytea NOT NULL,
    detection_data jsonb NOT NULL,
    inserted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    modified_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, id),
    UNIQUE (instance_id, entry_point_tracing_params_id)
)
PARTITION BY LIST (instance_id);

ALTER SEQUENCE tycho.entry_point_tracing_result_id_seq OWNED BY tycho.entry_point_tracing_result.id;

CREATE TABLE tycho.protocol_component_uses_entry_point (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    entry_point_id int8 NOT NULL,
    PRIMARY KEY (instance_id, protocol_component_id, entry_point_id)
)
PARTITION BY LIST (instance_id);

CREATE TABLE tycho.protocol_component_has_entry_point_tracing_params (
    instance_id int4 NOT NULL,
    protocol_component_id int8 NOT NULL,
    entry_point_tracing_params_id int8 NOT NULL,
    PRIMARY KEY (instance_id, protocol_component_id, entry_point_tracing_params_id)
)
PARTITION BY LIST (instance_id);

-- Triggers, matching the old tables. Row triggers on a partitioned parent are cloned to every
-- leaf when it is attached.

CREATE TRIGGER update_entry_point_tracing_params_data_hash_trigger
    BEFORE INSERT OR UPDATE ON tycho.entry_point_tracing_params
    FOR EACH ROW
    EXECUTE FUNCTION public.update_entry_point_tracing_params_data_hash();

DO $$
DECLARE
    v_table text;
BEGIN
    FOREACH v_table IN ARRAY ARRAY[
        'protocol_instance',
        'extraction_state',
        'protocol_component',
        'protocol_component_holds_token',
        'component_tvl',
        'account',
        'contract_code',
        'account_balance',
        'entry_point',
        'entry_point_tracing_params',
        'entry_point_tracing_result'
    ] LOOP
        EXECUTE format(
            'CREATE TRIGGER update_modtime_%1$s BEFORE UPDATE ON tycho.%1$I '
            'FOR EACH ROW EXECUTE FUNCTION public.update_modified_column()',
            v_table
        );
    END LOOP;
END;
$$;

-- History: one partition per day, registered with pg_partman and made ahead by
-- premake_history_partitions() below. Without a DEFAULT partition, a row for a day that hasn't
-- been made fails instead of landing elsewhere.
-- Each history table's PK leads with (instance_id, <key>, valid_to), the prefix point-in-time
-- reads use, and ends with (valid_from, tx_index): a transaction writes a key at most once, so a
-- duplicate means the same version was archived twice.

DO $$
DECLARE
    v_table text;
BEGIN
    FOREACH v_table IN ARRAY ARRAY[
        'tycho.protocol_state_history',
        'tycho.component_balance_history',
        'tycho.contract_code_history',
        'tycho.contract_storage_history',
        'tycho.account_balance_history'
    ] LOOP
        PERFORM partman.create_parent(
            p_parent_table := v_table,
            p_control := 'valid_to',
            p_interval := '1 day',
            p_type := 'range',
            p_premake := 7,
            p_default_table := false,
            p_automatic_maintenance := 'on'
        );
    END LOOP;
END;
$$;

-- Expired days are dropped, not just detached. Days are made by premake_history_partitions()
-- below instead of run_maintenance(): pg_partman 5.0 makes no new day for a partition set with no
-- rows, even with infinite_time_partitions, and these tables are empty until the data migration
-- (contract_code_history almost always).
UPDATE partman.part_config
SET retention_keep_table = false,
    automatic_maintenance = 'off'
WHERE parent_table LIKE 'tycho.%';

CREATE FUNCTION tycho.premake_history_partitions(p_days integer DEFAULT 7)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    v_parent text;
    v_days timestamptz[];
BEGIN
    SELECT array_agg(date_trunc('day', now()) + make_interval(days => d))
    INTO v_days
    FROM generate_series(0, p_days) AS d;
    FOREACH v_parent IN ARRAY ARRAY[
        'tycho.protocol_state_history',
        'tycho.component_balance_history',
        'tycho.contract_code_history',
        'tycho.contract_storage_history',
        'tycho.account_balance_history'
    ] LOOP
        -- Skips days that already exist.
        PERFORM partman.create_partition_time(v_parent, v_days);
    END LOOP;
END;
$$;

-- 00:15, after the midnight run_maintenance and before drop_expired_partitions at 00:30. A missed
-- run still leaves the rest of the 7-day runway.
SELECT cron.schedule(
    'premake_history_partitions',
    '15 0 * * *',
    'SELECT tycho.premake_history_partitions();'
);

-- Same procedure as in 2026-08-13_decouple_partition_retention, with the history parents added to
-- the swept tables.
CREATE OR REPLACE PROCEDURE drop_expired_partitions(
    p_retention interval DEFAULT NULL,
    p_lock_timeout text DEFAULT '2s',
    p_max_attempts integer DEFAULT 5
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_retention interval := coalesce(p_retention, partition_retention());
    v_parent text;
    v_attempt integer;
    v_done boolean;
    v_failed text[] := '{}';
BEGIN
    FOREACH v_parent IN ARRAY ARRAY[
        'public.component_balance',
        'public.contract_storage',
        'public.protocol_state',
        'tycho.protocol_state_history',
        'tycho.component_balance_history',
        'tycho.contract_code_history',
        'tycho.contract_storage_history',
        'tycho.account_balance_history'
    ] LOOP
        v_done := false;
        v_attempt := 0;
        WHILE NOT v_done AND v_attempt < p_max_attempts LOOP
            v_attempt := v_attempt + 1;
            BEGIN
                -- Applies to the current transaction only. If the parent lock is not granted
                -- within the timeout, give up and retry instead of stalling the queries that
                -- queue behind the AccessExclusiveLock request.
                PERFORM set_config('lock_timeout', p_lock_timeout, true);
                PERFORM partman.drop_partition_time(
                    p_parent_table := v_parent,
                    p_retention := v_retention
                );
                v_done := true;
            EXCEPTION
                -- Retry on any error, not just lock failures: partman re-raises internal
                -- errors with a generic SQLSTATE, so lock timeouts cannot be told apart from
                -- other failures here. The call is idempotent, and a persistent error still
                -- surfaces through the RAISE below after p_max_attempts.
                WHEN OTHERS THEN
                    RAISE NOTICE 'drop_expired_partitions: % failed (attempt % of %): %',
                        v_parent, v_attempt, p_max_attempts, SQLERRM;
            END;
            -- One transaction per attempt: releases everything before the backoff sleep or
            -- the next table.
            COMMIT;
            IF NOT v_done AND v_attempt < p_max_attempts THEN
                PERFORM pg_sleep(30);
            END IF;
        END LOOP;
        IF NOT v_done THEN
            v_failed := v_failed || v_parent;
        END IF;
    END LOOP;

    -- Completed drops are already committed; raising here only marks the run as failed in
    -- cron.job_run_details so persistent errors are observable.
    IF array_length(v_failed, 1) > 0 THEN
        RAISE EXCEPTION 'drop_expired_partitions: failed for % after % attempts each, remaining partitions retry next run',
            v_failed, p_max_attempts;
    END IF;
END;
$$;

-- Leaf builder. Every path that creates an instance (data migration, import, a fresh extractor)
-- calls both functions, so no instance misses its CHECK or its FKs. An import loads its data
-- between the two calls; the other paths call them back to back.

-- Per-instance tables, referenced tables before the tables that reference them.
CREATE FUNCTION tycho.instance_tables()
RETURNS text[]
LANGUAGE sql IMMUTABLE
AS $$
    SELECT ARRAY[
        'protocol_component',
        'account',
        'entry_point',
        'entry_point_tracing_params',
        'protocol_component_holds_token',
        'protocol_component_holds_contract',
        'protocol_state',
        'component_balance',
        'component_tvl',
        'contract_code',
        'contract_storage',
        'account_balance',
        'entry_point_tracing_params_calls_account',
        'entry_point_tracing_result',
        'protocol_component_uses_entry_point',
        'protocol_component_has_entry_point_tracing_params'
    ]
$$;

CREATE FUNCTION tycho.instance_leaf(p_table text, p_instance_id int4)
RETURNS text
LANGUAGE sql IMMUTABLE
AS $$
    SELECT p_table || '_i' || p_instance_id
$$;

-- Creates one standalone table per instance table, not yet attached and without keys or
-- indexes, so a bulk load into them is fast. instance_id defaults to the instance, so a COPY can
-- leave it out, and the CHECK lets ATTACH PARTITION skip its validation scan.
CREATE FUNCTION tycho.create_instance_leaves(p_instance_id int4)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    v_table text;
    v_leaf text;
    v_storage text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM tycho.protocol_instance WHERE id = p_instance_id) THEN
        RAISE EXCEPTION 'create_instance_leaves: instance % does not exist', p_instance_id;
    END IF;
    FOREACH v_table IN ARRAY tycho.instance_tables() LOOP
        v_leaf := tycho.instance_leaf(v_table, p_instance_id);
        -- Leave room for in-place (HOT) updates on the tables whose rows are upserted every block.
        v_storage := CASE
            WHEN v_table IN ('protocol_state', 'component_balance', 'contract_storage', 'account_balance')
                THEN ' WITH (fillfactor = 85)'
            ELSE ''
        END;
        EXECUTE format(
            'CREATE TABLE tycho.%I (LIKE tycho.%I INCLUDING DEFAULTS)%s',
            v_leaf, v_table, v_storage
        );
        EXECUTE format(
            'ALTER TABLE tycho.%I ALTER COLUMN instance_id SET DEFAULT %s',
            v_leaf, p_instance_id
        );
        EXECUTE format(
            'ALTER TABLE tycho.%I ADD CONSTRAINT %I CHECK (instance_id = %s)',
            v_leaf, v_leaf || '_instance_check', p_instance_id
        );
    END LOOP;
END;
$$;

-- Adds the parents' keys and indexes to the leaves, then the FKs between the leaves of this
-- instance, then attaches them. Keys and indexes are built before the attach so ATTACH reuses
-- them instead of building them under its lock. FKs point at the instance's own leaves only:
-- never across instances, never into shared tables, so detaching and dropping an instance locks
-- nothing outside it.
CREATE FUNCTION tycho.attach_instance_leaves(p_instance_id int4)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    v_table text;
    v_leaf text;
    v_def text;
    v_fk record;
BEGIN
    FOREACH v_table IN ARRAY tycho.instance_tables() LOOP
        v_leaf := tycho.instance_leaf(v_table, p_instance_id);
        FOR v_def IN
            SELECT pg_get_constraintdef(c.oid)
            FROM pg_constraint c
            WHERE c.conrelid = format('tycho.%I', v_table)::regclass
              AND c.contype IN ('p', 'u')
            ORDER BY c.contype, c.conname
        LOOP
            EXECUTE format('ALTER TABLE tycho.%I ADD %s', v_leaf, v_def);
        END LOOP;
        FOR v_def IN
            SELECT pg_get_indexdef(i.indexrelid)
            FROM pg_index i
            WHERE i.indrelid = format('tycho.%I', v_table)::regclass
              AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid)
            ORDER BY i.indexrelid
        LOOP
            EXECUTE regexp_replace(
                v_def,
                '^CREATE (UNIQUE )?INDEX \S+ ON ONLY \S+',
                'CREATE \1INDEX ON ' || format('tycho.%I', v_leaf)
            );
        END LOOP;
    END LOOP;

    FOR v_fk IN
        SELECT *
        FROM (VALUES
            ('protocol_component_holds_token', 'protocol_component_id', 'protocol_component'),
            ('protocol_component_holds_contract', 'protocol_component_id', 'protocol_component'),
            ('protocol_component_holds_contract', 'account_id', 'account'),
            ('protocol_state', 'protocol_component_id', 'protocol_component'),
            ('component_balance', 'protocol_component_id', 'protocol_component'),
            ('component_tvl', 'protocol_component_id', 'protocol_component'),
            ('contract_code', 'account_id', 'account'),
            ('contract_storage', 'account_id', 'account'),
            ('account_balance', 'account_id', 'account'),
            ('entry_point_tracing_params', 'entry_point_id', 'entry_point'),
            ('entry_point_tracing_params_calls_account', 'entry_point_tracing_params_id', 'entry_point_tracing_params'),
            ('entry_point_tracing_params_calls_account', 'account_id', 'account'),
            ('entry_point_tracing_result', 'entry_point_tracing_params_id', 'entry_point_tracing_params'),
            ('protocol_component_uses_entry_point', 'protocol_component_id', 'protocol_component'),
            ('protocol_component_uses_entry_point', 'entry_point_id', 'entry_point'),
            ('protocol_component_has_entry_point_tracing_params', 'protocol_component_id', 'protocol_component'),
            ('protocol_component_has_entry_point_tracing_params', 'entry_point_tracing_params_id', 'entry_point_tracing_params')
        ) AS fk (from_table, from_column, to_table)
    LOOP
        EXECUTE format(
            'ALTER TABLE tycho.%I ADD FOREIGN KEY (instance_id, %I) REFERENCES tycho.%I (instance_id, id)',
            tycho.instance_leaf(v_fk.from_table, p_instance_id),
            v_fk.from_column,
            tycho.instance_leaf(v_fk.to_table, p_instance_id)
        );
    END LOOP;

    FOREACH v_table IN ARRAY tycho.instance_tables() LOOP
        EXECUTE format(
            'ALTER TABLE tycho.%I ATTACH PARTITION tycho.%I FOR VALUES IN (%s)',
            v_table, tycho.instance_leaf(v_table, p_instance_id), p_instance_id
        );
    END LOOP;
END;
$$;

-- The changes to shared tables come last: ADD COLUMN takes an ACCESS EXCLUSIVE lock, held until
-- the migration commits, and token is read all the time.

-- The only record of which instance readers see. Several names may point at one instance later
-- (aliases), so this is not unique.
ALTER TABLE public.protocol_system
    ADD COLUMN active_instance_id int4 REFERENCES tycho.protocol_instance (id);

-- Instance tables refer to tokens by address. Made NOT NULL and UNIQUE by the data migration,
-- once every row is filled.
ALTER TABLE public.token
    ADD COLUMN address bytea;
