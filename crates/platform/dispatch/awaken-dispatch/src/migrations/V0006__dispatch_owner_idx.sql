-- index: renew all leases held by one owner
CREATE INDEX {prefix}_dispatch_owner_idx ON {prefix}_dispatch (lease_owner)
