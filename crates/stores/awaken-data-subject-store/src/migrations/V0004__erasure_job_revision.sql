-- Optimistic-concurrency fence for multi-replica erasure saga checkpoints
ALTER TABLE {prefix}_erasure_job
ADD COLUMN revision BIGINT NOT NULL DEFAULT 0
