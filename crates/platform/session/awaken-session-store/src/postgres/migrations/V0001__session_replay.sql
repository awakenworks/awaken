CREATE TABLE {prefix}_session_revision (
    scope_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES {prefix}_session(session_id) ON DELETE CASCADE,
    revision BIGINT NOT NULL,
    aggregate_json TEXT NOT NULL,
    PRIMARY KEY (scope_id, session_id, revision)
);

CREATE INDEX {prefix}_session_revision_cleanup ON {prefix}_session_revision (session_id);

CREATE TABLE {prefix}_session_replay_point (
    scope_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    point_id TEXT NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    revision BIGINT NOT NULL,
    runtime_through BIGINT NOT NULL CHECK (runtime_through >= 0),
    PRIMARY KEY (scope_id, session_id, point_id),
    FOREIGN KEY (scope_id, session_id, revision)
        REFERENCES {prefix}_session_revision(scope_id, session_id, revision) ON DELETE CASCADE
);
