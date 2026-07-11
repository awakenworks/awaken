-- index: status-scoped claim ordering (fresh pick, parked wake, status scans)
CREATE INDEX {prefix}_dispatch_claim_idx
    ON {prefix}_dispatch (status, priority, created_at)
