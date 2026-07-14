-- fencing token: a monotone epoch bumped on every claim, so a stale owner whose
-- lease lapsed cannot settle the dispatch out from under a reclaimer (ADR-0022 fence)
ALTER TABLE {prefix}_dispatch ADD COLUMN lease_epoch BIGINT NOT NULL DEFAULT 0;
