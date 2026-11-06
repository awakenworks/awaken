-- versioned MemoryStore and Repository catalog records (secret-free; configs retain immutable history)
CREATE TABLE {prefix}_resource_catalog (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now},
    PRIMARY KEY (kind, id)
)
