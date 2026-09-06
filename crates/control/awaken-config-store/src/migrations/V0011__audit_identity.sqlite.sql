-- Append V11 to the original owner. Decode the historical record, never split
-- the ambiguous legacy tool:call_id key. Rebuild atomically, with no dual reads.
CREATE TABLE {prefix}_management_audit_identity (
    scope_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    call_id TEXT NOT NULL,
    record TEXT NOT NULL,
    business_committed BIGINT NOT NULL DEFAULT 0 CHECK (business_committed IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, tool, call_id),
    CHECK (json_type(record, '$.tool') IS 'text' AND tool = json_extract(record, '$.tool')),
    CHECK (json_type(record, '$.call_id') IS 'text' AND call_id = json_extract(record, '$.call_id')),
    CHECK (json_type(record, '$.summary') IS 'text')
);
INSERT INTO {prefix}_management_audit_identity
    (scope_id, tool, call_id, record, business_committed, created_at)
SELECT scope_id, json_extract(record, '$.tool'), json_extract(record, '$.call_id'),
       record, business_committed, created_at FROM {prefix}_management_audit;
DROP TABLE {prefix}_management_audit;
ALTER TABLE {prefix}_management_audit_identity RENAME TO {prefix}_management_audit;
