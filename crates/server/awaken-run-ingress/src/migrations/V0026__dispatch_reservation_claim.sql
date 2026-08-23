-- include Session reservation recovery in the existing per-Thread claim fence
DROP INDEX {prefix}_dispatch_one_running_idx;
CREATE UNIQUE INDEX {prefix}_dispatch_one_running_idx
    ON {prefix}_dispatch (thread_id)
    WHERE status IN ('running', 'reservation_running')
