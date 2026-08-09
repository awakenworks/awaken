-- reject negative durable authority counters at the storage boundary
ALTER TABLE {prefix}_dispatch
    ADD CONSTRAINT {prefix}_dispatch_nonnegative_authority
    CHECK (lease_epoch >= 0 AND attempt_count >= 0 AND epoch >= 0);

ALTER TABLE {prefix}_pending
    ADD CONSTRAINT {prefix}_pending_nonnegative_revision
    CHECK (revision >= 0);
