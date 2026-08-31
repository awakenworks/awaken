-- retain one exact physical executor until it acknowledges quiescence
ALTER TABLE {prefix}_dispatch
    ADD COLUMN active_attempt_owner TEXT,
    ADD COLUMN active_attempt_epoch BIGINT;

ALTER TABLE {prefix}_dispatch
    ADD CONSTRAINT {prefix}_dispatch_attempt_slot_pair
    CHECK (
        (active_attempt_owner IS NULL AND active_attempt_epoch IS NULL)
        OR
        (active_attempt_owner IS NOT NULL AND active_attempt_epoch IS NOT NULL
            AND active_attempt_epoch >= 0)
    );
