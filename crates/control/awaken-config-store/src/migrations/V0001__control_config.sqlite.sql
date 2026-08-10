-- complete Control Agent config, publication, audit, and immutable revision schema
CREATE TABLE {prefix}_agent (
    id TEXT NOT NULL,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    scope_id TEXT NOT NULL DEFAULT 'default',
    generation BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (scope_id, id)
);
CREATE TABLE {prefix}_publication (
    fingerprint TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL,
    record TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    scope_id TEXT NOT NULL DEFAULT 'default',
    PRIMARY KEY (scope_id, fingerprint)
);
CREATE INDEX {prefix}_publication_created_at_idx
ON {prefix}_publication (created_at);
CREATE TABLE {prefix}_management_audit (
    scope_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    record TEXT NOT NULL,
    business_committed BIGINT NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, call_id)
);
CREATE TABLE {prefix}_management_effect (
    scope_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    effect_key TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, kind, effect_key)
);
CREATE TABLE {prefix}_agent_revision (
    scope_id TEXT NOT NULL,
    id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, id, generation)
);
CREATE TRIGGER {prefix}_agent_revision_insert
AFTER INSERT ON {prefix}_agent
BEGIN
    INSERT INTO {prefix}_agent_revision (scope_id, id, generation, data)
    VALUES (NEW.scope_id, NEW.id, NEW.generation, NEW.data);
END;
CREATE TRIGGER {prefix}_agent_revision_update
AFTER UPDATE ON {prefix}_agent
WHEN OLD.generation <> NEW.generation
BEGIN
    INSERT INTO {prefix}_agent_revision (scope_id, id, generation, data)
    VALUES (NEW.scope_id, NEW.id, NEW.generation, NEW.data);
END
