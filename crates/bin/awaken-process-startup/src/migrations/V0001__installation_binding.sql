CREATE TABLE {prefix}_binding (
    singleton SMALLINT PRIMARY KEY CHECK (singleton = 1),
    platform_workspace_id TEXT NOT NULL CHECK (
        length(platform_workspace_id) > 0
        AND platform_workspace_id = trim(platform_workspace_id)
    ),
    binding_origin TEXT NOT NULL CHECK (
        binding_origin IN ('fresh_initialization', 'legacy_adoption')
    ),
    operator_reference TEXT NOT NULL CHECK (
        length(operator_reference) > 0
        AND operator_reference = trim(operator_reference)
    ),
    bound_at {timestamptz} NOT NULL DEFAULT {now}
)
