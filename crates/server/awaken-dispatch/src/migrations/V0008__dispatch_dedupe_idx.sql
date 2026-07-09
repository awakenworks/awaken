-- index: dedupe-key existence check on enqueue
CREATE INDEX {prefix}_dispatch_dedupe_idx ON {prefix}_dispatch (dedupe_key)
