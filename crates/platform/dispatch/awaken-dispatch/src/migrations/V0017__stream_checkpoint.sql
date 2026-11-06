-- best-effort durable in-flight stream checkpoints for remote workers
CREATE TABLE {prefix}_stream_checkpoint (
    run_id TEXT PRIMARY KEY,
    checkpoint {json} NOT NULL
)
