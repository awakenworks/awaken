-- data subjects: the attributed party, its consent grants, keyed by org
CREATE TABLE {prefix}_subject (
    id TEXT PRIMARY KEY,
    org TEXT NOT NULL,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
