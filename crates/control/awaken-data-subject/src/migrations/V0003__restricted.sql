-- Art. 18 restriction: a restricted row is exempt from erasure + TTL sweep
ALTER TABLE {prefix}_captured ADD COLUMN restricted INTEGER NOT NULL DEFAULT 0
