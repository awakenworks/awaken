-- attributed Managed Memory version mutations
ALTER TABLE {prefix}_versions ADD COLUMN created_by_json TEXT;
ALTER TABLE {prefix}_versions ADD COLUMN redacted_by_json TEXT;
