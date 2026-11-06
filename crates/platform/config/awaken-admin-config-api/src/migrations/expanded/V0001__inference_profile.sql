-- inference profiles: authored admin-plane aggregates, one JSON row per id
CREATE TABLE {prefix}_inference_profile (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
