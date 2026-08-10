-- retain cancellation intent until the fenced worker commits and settles the run
ALTER TABLE {prefix}_dispatch ADD COLUMN cancel_requested BIGINT NOT NULL DEFAULT 0;
