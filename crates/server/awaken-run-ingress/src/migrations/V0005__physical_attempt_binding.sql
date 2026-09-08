-- Freeze the execution provider coordinate in the existing physical slot.
-- Legacy occupied slots remain unbound; migration cannot manufacture evidence.
ALTER TABLE {prefix}_dispatch ADD COLUMN active_attempt_binding TEXT
    CHECK (active_attempt_binding IS NULL OR
        (active_attempt_owner IS NOT NULL AND active_attempt_epoch IS NOT NULL));
