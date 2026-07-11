-- run-dispatch queue: one row per accepted run with claim/lease state
CREATE TABLE {prefix}_dispatch (
    run_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    request {json} NOT NULL,
    status TEXT NOT NULL,
    lease_owner TEXT,
    lease_until BIGINT,
    attempt_count BIGINT NOT NULL DEFAULT 0,
    priority BIGINT NOT NULL DEFAULT 0,
    epoch BIGINT NOT NULL DEFAULT 0,
    dead_lettered_at BIGINT,
    dedupe_key TEXT,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
