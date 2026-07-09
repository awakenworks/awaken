-- index: a thread's pending inbox listing
CREATE INDEX {prefix}_pending_thread_idx ON {prefix}_pending (thread_id)
