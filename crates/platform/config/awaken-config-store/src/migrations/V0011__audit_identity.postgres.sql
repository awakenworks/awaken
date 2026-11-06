-- The same V11 is appended to both published histories of the config owner.
CREATE TABLE {prefix}_management_audit_identity (
    scope_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    call_id TEXT NOT NULL,
    record JSONB NOT NULL,
    business_committed BIGINT NOT NULL DEFAULT 0 CHECK (business_committed IN (0, 1)),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (scope_id, tool, call_id),
    CHECK ((jsonb_typeof(record->'tool') = 'string') IS TRUE AND tool = record->>'tool'),
    CHECK ((jsonb_typeof(record->'call_id') = 'string') IS TRUE AND call_id = record->>'call_id'),
    CHECK ((jsonb_typeof(record->'summary') = 'string') IS TRUE)
);
INSERT INTO {prefix}_management_audit_identity
    (scope_id, tool, call_id, record, business_committed, created_at)
SELECT scope_id, record->>'tool', record->>'call_id', record, business_committed, created_at
FROM {prefix}_management_audit;
DROP TABLE {prefix}_management_audit;
ALTER TABLE {prefix}_management_audit_identity RENAME TO {prefix}_management_audit;
