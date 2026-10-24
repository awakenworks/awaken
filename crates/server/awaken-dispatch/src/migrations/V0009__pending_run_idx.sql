-- index: a run's pending input (claim hand-off, settle, cancel, wake test)
CREATE INDEX {prefix}_pending_run_idx ON {prefix}_pending (run_id)
