ALTER TABLE {prefix}_item ADD COLUMN lease_owner TEXT; ALTER TABLE {prefix}_item ADD COLUMN lease_epoch BIGINT NOT NULL DEFAULT 0; ALTER TABLE {prefix}_item ADD COLUMN lease_expires_ms BIGINT
