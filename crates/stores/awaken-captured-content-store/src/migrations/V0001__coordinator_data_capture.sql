-- Coordinator-owned subject-tagged content with TTL, erasure, and Art. 18 restriction state
CREATE TABLE {prefix}_captured (
    id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    purpose TEXT NOT NULL,
    recorded_at BIGINT NOT NULL,
    content TEXT NOT NULL,
    restricted INTEGER NOT NULL DEFAULT 0
)
