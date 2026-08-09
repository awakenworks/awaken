-- create the complete Control-owned admin authoring schema
CREATE TABLE {prefix}_inference_profile (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
);
CREATE TABLE {prefix}_agent_resource (
    agent_id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
);
CREATE TABLE {prefix}_webhook (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
