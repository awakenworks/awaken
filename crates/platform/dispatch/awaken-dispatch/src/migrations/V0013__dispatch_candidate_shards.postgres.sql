ALTER TABLE {prefix}_dispatch
ADD COLUMN dispatch_shard SMALLINT GENERATED ALWAYS AS (
    get_byte(decode(md5(
        COALESCE(request->>'execution_scope', '') || chr(31) ||
        COALESCE(request->>'session_thread_id', thread_id)
    ), 'hex'), 0)::smallint
) STORED;

CREATE INDEX {prefix}_dispatch_shard_ready_idx
    ON {prefix}_dispatch (dispatch_shard, status, priority DESC, created_at, run_id)
    WHERE status IN ('pending', 'awaiting');

CREATE INDEX {prefix}_dispatch_shard_expiry_idx
    ON {prefix}_dispatch (dispatch_shard, lease_until)
    WHERE status IN ('reserved', 'reservation_running', 'running');
