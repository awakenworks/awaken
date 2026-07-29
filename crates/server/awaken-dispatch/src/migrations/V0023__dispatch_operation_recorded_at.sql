-- store-owned wall-clock time for durable dispatch authority mutations
ALTER TABLE {prefix}_dispatch_operation
ADD COLUMN recorded_at_ms BIGINT
