-- atomic memory version journal and durable id high-water marks
CREATE TABLE {prefix}_counters (
    name TEXT NOT NULL,
    next_value BIGINT NOT NULL,
    PRIMARY KEY (name)
);

CREATE TABLE {prefix}_versions (
    store_id TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    id TEXT NOT NULL,
    memory_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    path TEXT NOT NULL,
    content {blob},
    created BIGINT NOT NULL,
    redacted BIGINT,
    PRIMARY KEY (store_id, ordinal),
    UNIQUE (id)
);
