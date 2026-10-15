-- Fencing token for the single database writer. The process that becomes the writer increments
-- `epoch`; every write transaction checks that the value is still the one it claimed.
CREATE TABLE writer_epoch (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    epoch bigint NOT NULL
);

INSERT INTO writer_epoch (id, epoch) VALUES (true, 0);
