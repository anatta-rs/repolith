CREATE TABLE IF NOT EXISTS build_events (
    action_id   TEXT PRIMARY KEY,
    input_hash  TEXT NOT NULL,
    output_hash TEXT,
    status      TEXT NOT NULL CHECK(status IN ('success','failed')),
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER NOT NULL,
    error_json  TEXT
);
CREATE INDEX IF NOT EXISTS idx_status ON build_events(status);
