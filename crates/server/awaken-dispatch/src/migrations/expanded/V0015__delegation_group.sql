-- durable parent-child Run coordination with optimistic-concurrency fencing
CREATE TABLE {prefix}_delegation_group (
    parent_run_id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL DEFAULT 0,
    group_json {json} NOT NULL
)
