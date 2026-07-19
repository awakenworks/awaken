-- index: per-thread supersession and awaiting-run lookup
CREATE INDEX {prefix}_dispatch_thread_idx ON {prefix}_dispatch (thread_id)
