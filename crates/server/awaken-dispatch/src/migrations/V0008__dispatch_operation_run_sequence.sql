-- index exact Run operational replay over the existing global event identities
CREATE INDEX {prefix}_dispatch_operation_run_sequence
ON {prefix}_dispatch_operation (run_id, sequence)
