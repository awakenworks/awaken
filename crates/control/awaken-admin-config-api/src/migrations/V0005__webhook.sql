-- webhook endpoints: authored WebhookEndpointDef rows (secret-free; the whsec_ signing key is sealed in the SecretStore, the row carries only its secret_ref)
CREATE TABLE {prefix}_webhook (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
