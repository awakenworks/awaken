ALTER TABLE {prefix}_session_revision ADD COLUMN recorded_at_ms BIGINT;
ALTER TABLE {prefix}_session_revision ADD COLUMN configuration_changes JSONB;

CREATE TABLE {prefix}_session_diagnostic_key (
    singleton BOOLEAN PRIMARY KEY CHECK (singleton),
    key_hex TEXT NOT NULL
);
INSERT INTO {prefix}_session_diagnostic_key (singleton, key_hex)
VALUES (TRUE, replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', ''));
