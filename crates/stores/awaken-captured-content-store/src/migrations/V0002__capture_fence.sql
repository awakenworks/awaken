-- Coordinator capture fence for erased data subjects
CREATE TABLE {prefix}_fence (
    subject TEXT PRIMARY KEY,
    erased_at BIGINT NOT NULL,
    records_removed BIGINT NOT NULL CHECK (records_removed >= 0)
)
