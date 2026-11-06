-- replace the legacy JSON snapshot with one constrained relational worker authority
DROP INDEX {prefix}_expiry_idx;
CREATE TABLE {prefix}_worker_v3 (
    worker_id TEXT PRIMARY KEY CHECK (length(worker_id) > 0),
    incarnation_id TEXT NOT NULL CHECK (length(incarnation_id) > 0),
    generation BIGINT NOT NULL CHECK (generation >= 0),
    state TEXT NOT NULL CHECK (state IN ('starting', 'ready', 'draining', 'quiesced', 'dead')),
    manifest_json TEXT NOT NULL,
    capability_fingerprint TEXT NOT NULL,
    in_flight BIGINT NOT NULL CHECK (in_flight >= 0 AND in_flight <= 4294967295),
    warm_environment_shapes_json TEXT NOT NULL,
    credential_observations_json TEXT NOT NULL,
    acp_capability_observations_json TEXT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms >= 0),
    heartbeat_sequence BIGINT NOT NULL CHECK (heartbeat_sequence >= 0),
    observation_sequence BIGINT NOT NULL CHECK (observation_sequence >= 0),
    registered_at_ms BIGINT NOT NULL CHECK (registered_at_ms >= 0),
    heartbeat_at_ms BIGINT NOT NULL CHECK (heartbeat_at_ms >= 0),
    drain_deadline_ms BIGINT CHECK (drain_deadline_ms IS NULL OR drain_deadline_ms >= 0)
);
INSERT INTO {prefix}_worker_v3 (
    worker_id, incarnation_id, generation, state, manifest_json,
    capability_fingerprint, in_flight, warm_environment_shapes_json,
    credential_observations_json, acp_capability_observations_json,
    expires_at_ms, heartbeat_sequence, observation_sequence,
    registered_at_ms, heartbeat_at_ms, drain_deadline_ms
)
SELECT
    worker_id,
    incarnation_id,
    generation,
    json_extract(record_json, '$.snapshot.state'),
    json_extract(record_json, '$.snapshot.manifest'),
    json_extract(record_json, '$.snapshot.capability_fingerprint'),
    COALESCE(json_extract(record_json, '$.snapshot.in_flight'), 0),
    COALESCE(json_extract(record_json, '$.snapshot.warm_environment_shapes'), json('[]')),
    COALESCE(json_extract(record_json, '$.snapshot.credential_observations'), json('[]')),
    COALESCE(json_extract(record_json, '$.snapshot.acp_capability_observations'), json('[]')),
    expires_at_ms,
    COALESCE(json_extract(record_json, '$.heartbeat_sequence'), 0),
    COALESCE(json_extract(record_json, '$.observation_sequence'), 0),
    COALESCE(json_extract(record_json, '$.registered_at_ms'), 0),
    COALESCE(json_extract(record_json, '$.heartbeat_at_ms'), 0),
    json_extract(record_json, '$.drain_deadline_ms')
FROM {prefix}_worker;
DROP TABLE {prefix}_worker;
ALTER TABLE {prefix}_worker_v3 RENAME TO {prefix}_worker;
CREATE INDEX {prefix}_expiry_idx ON {prefix}_worker (state, expires_at_ms)
