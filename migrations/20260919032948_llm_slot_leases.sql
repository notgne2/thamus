CREATE TABLE llm_slot_leases (
    slot_id          INTEGER    PRIMARY KEY,
    in_use           INTEGER    NOT NULL DEFAULT 0,
    acquired_at      TEXT,                      -- LRU key; kept while free
    last_dream       INTEGER,
    last_instance_id TEXT,
    -- Outgoing holder, stashed during acquire so the caller can be told who
    -- had the slot before this acquisition.
    prev_dream       INTEGER,
    prev_instance_id TEXT
);