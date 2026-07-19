-- durable GDPR erasure workflow checkpoints for crash-safe fan-out recovery
CREATE TABLE {prefix}_erasure_job (
    subject_id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    updated_at {timestamptz} NOT NULL DEFAULT {now}
)
