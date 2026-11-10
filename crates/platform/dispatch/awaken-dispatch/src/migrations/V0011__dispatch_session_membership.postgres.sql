-- Preserve diagnostic membership past removal of the live queue row.
CREATE TABLE {prefix}_dispatch_session_membership (
    run_id TEXT PRIMARY KEY,
    scope_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    sequence BIGINT NOT NULL UNIQUE,
    recorded_at_ms BIGINT NOT NULL,
    initial_status TEXT NOT NULL,
    parent_run_id TEXT
);
CREATE INDEX {prefix}_dispatch_membership_session
    ON {prefix}_dispatch_session_membership (scope_id, session_id, sequence);
