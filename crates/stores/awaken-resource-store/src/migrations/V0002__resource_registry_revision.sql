-- add backend-neutral optimistic concurrency to Resource Registry aggregates
ALTER TABLE {prefix}_entry
ADD COLUMN revision BIGINT NOT NULL DEFAULT 1
