-- captured content: subject-tagged, erasable + TTL-swept telemetry content
CREATE TABLE {prefix}_captured (
    id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    purpose TEXT NOT NULL,
    recorded_at BIGINT NOT NULL,
    content TEXT NOT NULL
)
