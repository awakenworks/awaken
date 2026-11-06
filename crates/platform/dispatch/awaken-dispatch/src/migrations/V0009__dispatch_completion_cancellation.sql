-- retain on the completion tombstone whether a durable cancel was requested before settle
ALTER TABLE {prefix}_dispatch_completion ADD COLUMN cancel_requested BIGINT NOT NULL DEFAULT 0
