-- Index frozen Thread history and exact committed evidence lookups.
CREATE INDEX {prefix}_message_visible_position ON {prefix}_message (thread_id, id)
    WHERE data->>'role' IN ('User', 'Assistant');
CREATE INDEX {prefix}_message_thread_commit ON {prefix}_message (thread_id, commit_sequence, id);
CREATE INDEX {prefix}_state_command_thread_commit ON {prefix}_state_command (thread_id, commit_sequence, id);
CREATE INDEX {prefix}_commit_receipt_commit_sequence ON {prefix}_commit_receipt (commit_sequence);
CREATE INDEX {prefix}_run_activation_subject ON {prefix}_run_activation (scope_id, session_id, run_id);
CREATE INDEX {prefix}_run_activation_parent ON {prefix}_run_activation
    (scope_id, session_id, (data->'activation'->'delegation_origin'->>'parent_run_id'), first_commit_sequence);
