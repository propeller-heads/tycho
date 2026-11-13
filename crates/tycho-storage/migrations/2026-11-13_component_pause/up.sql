-- Admin pauses of protocol components. A pause is closed by setting unpaused_at, never deleted,
-- so closed rows keep the history of every pause.
CREATE TABLE IF NOT EXISTS component_pause (
    id                 BIGSERIAL PRIMARY KEY,
    chain_id           BIGINT NOT NULL REFERENCES chain(id),
    protocol_system_id BIGINT NOT NULL REFERENCES protocol_system(id),
    -- External id of the component. No foreign key, so an id can be paused before it is indexed.
    component_id       VARCHAR(255) NOT NULL,
    -- Operator audit fields. Nothing reads them to make a decision.
    reason             TEXT,
    actor              TEXT,
    evidence_url       TEXT,
    unpause_actor      TEXT,
    unpause_reason     TEXT,
    paused_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    unpaused_at        TIMESTAMPTZ,
    inserted_ts        TIMESTAMPTZ NOT NULL DEFAULT now(),
    modified_ts        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- At most one open pause per component. A component id is unique per chain, so the protocol
-- system is not part of the key.
CREATE UNIQUE INDEX IF NOT EXISTS component_pause_open_idx
    ON component_pause (chain_id, component_id)
    WHERE unpaused_at IS NULL;

CREATE TRIGGER update_modtime_component_pause
    BEFORE UPDATE ON component_pause
    FOR EACH ROW
    EXECUTE PROCEDURE update_modified_column();
