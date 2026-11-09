-- Native Account-owned preferences contain no Agent definition or authority.
CREATE TABLE {prefix}_personal_agent_selection (
    scope_id TEXT NOT NULL,
    issuer TEXT NOT NULL,
    account_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    selected BIGINT NOT NULL CHECK (selected IN (0, 1)),
    PRIMARY KEY (scope_id, issuer, account_id, agent_id)
);
CREATE TABLE {prefix}_personal_agent_selection_receipt (
    scope_id TEXT NOT NULL,
    issuer TEXT NOT NULL,
    account_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    request_key TEXT NOT NULL,
    record TEXT NOT NULL,
    PRIMARY KEY (scope_id, issuer, account_id, agent_id, request_key)
)
