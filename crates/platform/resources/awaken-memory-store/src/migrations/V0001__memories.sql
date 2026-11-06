-- path-addressed memories: (store_id, path) keyed, with content_sha256 + version (ADR-0053)
CREATE TABLE {prefix}_memories (
    store_id TEXT NOT NULL,
    path TEXT NOT NULL,
    id TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    content {blob} NOT NULL,
    sha TEXT NOT NULL,
    version BIGINT NOT NULL,
    created BIGINT NOT NULL,
    updated BIGINT NOT NULL,
    PRIMARY KEY (store_id, path)
)
