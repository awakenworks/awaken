-- Isolate request-specific resolution retry from unrelated dispatches.
ALTER TABLE {prefix}_dispatch ADD COLUMN retry_not_before BIGINT;

CREATE INDEX {prefix}_dispatch_resolution_retry_idx
    ON {prefix}_dispatch (status, retry_not_before, priority, created_at);
