-- retain logical Thread and parent Session affinity on completion tombstones
ALTER TABLE {prefix}_dispatch_completion ADD COLUMN thread_id TEXT;
ALTER TABLE {prefix}_dispatch_completion ADD COLUMN session_thread_id TEXT;
CREATE INDEX {prefix}_dispatch_completion_session_thread_idx
    ON {prefix}_dispatch_completion (session_thread_id, thread_id)
