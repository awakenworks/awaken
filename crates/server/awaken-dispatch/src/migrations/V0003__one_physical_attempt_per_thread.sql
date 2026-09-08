-- reject two physical execution owners on one Thread regardless of dispatch status
CREATE UNIQUE INDEX {prefix}_dispatch_one_physical_attempt_idx
    ON {prefix}_dispatch (thread_id)
    WHERE active_attempt_owner IS NOT NULL
