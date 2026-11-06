CREATE TABLE IF NOT EXISTS {prefix}_version (policy_id TEXT NOT NULL, version BIGINT NOT NULL, policy_json TEXT NOT NULL, PRIMARY KEY(policy_id, version))
