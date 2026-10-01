-- SQLite already serializes writers, including historical publication rows.
ALTER TABLE {prefix}_publication
ADD COLUMN order_verified INTEGER NOT NULL DEFAULT 1 CHECK (order_verified IN (0, 1))
