-- persist the worker incarnation and capability fingerprint selected for a lease epoch
ALTER TABLE {prefix}_dispatch ADD COLUMN worker_assignment {json};
