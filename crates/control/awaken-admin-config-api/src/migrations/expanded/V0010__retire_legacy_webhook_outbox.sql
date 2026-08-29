-- retire the unused admin webhook outbox; Session lifecycle facts are the sole delivery authority
-- migration-allow-edit: adopt the scoped ledger as the sole idempotency mechanism
DROP TABLE {prefix}_webhook_outbox;
