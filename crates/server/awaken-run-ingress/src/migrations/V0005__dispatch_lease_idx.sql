-- index: expired-lease recovery and reap by lease deadline
CREATE INDEX {prefix}_dispatch_lease_idx
    ON {prefix}_dispatch (status, lease_until)
