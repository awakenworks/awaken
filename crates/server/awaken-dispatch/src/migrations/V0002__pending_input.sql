-- thread pending input, delivered to the matching waiting-ticket correlation
CREATE TABLE {prefix}_pending (
    message_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    correlation_id TEXT NOT NULL,
    result {json} NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    available_at BIGINT,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
