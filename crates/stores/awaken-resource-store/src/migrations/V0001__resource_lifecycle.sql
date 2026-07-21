-- durable purge intents, intrinsic references, and physical-reclamation fences
CREATE TABLE {prefix}_purge_intents (
    intent_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    revision BIGINT NOT NULL,
    status TEXT NOT NULL,
    requested_at_unix_ms BIGINT NOT NULL,
    not_before_unix_ms BIGINT NOT NULL,
    lease_expires_at_unix_ms BIGINT,
    data TEXT NOT NULL
);

CREATE INDEX {prefix}_purge_recoverable
    ON {prefix}_purge_intents(status, not_before_unix_ms, lease_expires_at_unix_ms);

CREATE TABLE {prefix}_references (
    workspace_id TEXT NOT NULL,
    resource_kind TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    reference_id TEXT NOT NULL,
    PRIMARY KEY(workspace_id, resource_kind, resource_id, reference_kind, reference_id)
);

CREATE INDEX {prefix}_references_reverse
    ON {prefix}_references(resource_kind, resource_id);

CREATE TABLE {prefix}_reclamation_fences (
    resource_kind TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    intent_id TEXT NOT NULL,
    PRIMARY KEY(resource_kind, resource_id)
);
