-- memory blobs: id-keyed bytes per workspace, with a dense-id ordinal
CREATE TABLE {prefix}_blob (
    workspace_id TEXT NOT NULL,
    id TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    content {blob} NOT NULL,
    PRIMARY KEY (workspace_id, id)
)
