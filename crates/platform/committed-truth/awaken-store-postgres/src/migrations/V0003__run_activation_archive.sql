CREATE TABLE {prefix}_run_activation (
    scope_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    first_commit_sequence BIGINT NOT NULL REFERENCES {prefix}_commit(sequence),
    data JSONB NOT NULL,
    PRIMARY KEY (scope_id, session_id, thread_id, run_id)
);
