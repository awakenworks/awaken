-- durable applied-Done completion log and permanent run-id tombstone
CREATE TABLE {prefix}_dispatch_completion (
    sequence {pk_autoinc},
    run_id TEXT NOT NULL UNIQUE
)
