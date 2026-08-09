-- Control-owned data-subject aggregate rows, keyed by organization
CREATE TABLE {prefix}_subject (
    id TEXT PRIMARY KEY,
    org TEXT NOT NULL,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
