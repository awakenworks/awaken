-- memory stores: authored MemoryStoreDef identity rows (secret-free; content lives in the data-plane MemoryFs)
CREATE TABLE {prefix}_memory_store (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
