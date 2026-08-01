-- complete binary-safe Skill aggregate; replaces the V1 current-SKILL.md projection
CREATE TABLE {prefix}_aggregate (
    workspace_id TEXT NOT NULL,
    id TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (workspace_id, id)
)
