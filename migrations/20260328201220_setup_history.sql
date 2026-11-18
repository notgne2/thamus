CREATE TABLE history (
    "id" INTEGER PRIMARY KEY AUTOINCREMENT,

    "instance_id" TEXT NOT NULL,

    -- `sorting` is a tie-breaker used only within the same `created_at`.
    "sorting" INTEGER NOT NULL DEFAULT 0,

    "json" TEXT NOT NULL,

    "created_at" TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX history_instance_created_idx ON history (instance_id, created_at, sorting);