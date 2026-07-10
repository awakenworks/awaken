-- constraint: at most one running dispatch per thread (single-writer, ADR-0022)
CREATE UNIQUE INDEX {prefix}_dispatch_one_running_idx
    ON {prefix}_dispatch (thread_id)
    WHERE status = 'running'
