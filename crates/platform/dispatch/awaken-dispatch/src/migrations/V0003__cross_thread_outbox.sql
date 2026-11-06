-- cross-thread outbox: staged deliveries awaiting relay to a target thread
CREATE TABLE {prefix}_outbox (
    message_id TEXT PRIMARY KEY,
    payload {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
