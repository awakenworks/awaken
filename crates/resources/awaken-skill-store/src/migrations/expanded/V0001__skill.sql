-- delivered skills: SKILL.md bodies, keyed by (workspace, id)
CREATE TABLE {prefix}_skill (
    workspace_id TEXT NOT NULL,
    id TEXT NOT NULL,
    content TEXT NOT NULL,
    PRIMARY KEY (workspace_id, id)
)
