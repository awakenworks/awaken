-- index: per-thread supersession and parked-run lookup
CREATE INDEX {prefix}_dispatch_thread_idx ON {prefix}_dispatch (thread_id)
