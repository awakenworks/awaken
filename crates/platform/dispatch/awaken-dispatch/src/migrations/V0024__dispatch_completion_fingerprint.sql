-- bind permanent completion tombstones to the accepted dispatch identity
ALTER TABLE {prefix}_dispatch_completion ADD COLUMN request_fingerprint TEXT
