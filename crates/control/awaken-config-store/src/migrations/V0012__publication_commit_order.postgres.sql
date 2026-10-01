-- Existing rows predate Workspace-wide publication commit serialization.
ALTER TABLE {prefix}_publication
ADD COLUMN order_verified BOOLEAN NOT NULL DEFAULT FALSE
