-- durable lifecycle webhook outbox keyed by stable logical event id
CREATE TABLE {prefix}_webhook_outbox (
    event_id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
