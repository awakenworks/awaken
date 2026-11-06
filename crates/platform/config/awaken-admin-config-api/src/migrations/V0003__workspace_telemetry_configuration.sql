-- Workspace telemetry narrowing beneath the deployment capture ceiling
CREATE TABLE {prefix}_telemetry_configuration (
    workspace_id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL CHECK (revision > 0),
    data {json} NOT NULL
)
