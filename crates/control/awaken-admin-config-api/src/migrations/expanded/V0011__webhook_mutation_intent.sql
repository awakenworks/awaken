-- durable webhook material mutation intents for cross-store create/delete recovery
CREATE TABLE {prefix}_webhook_mutation (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
