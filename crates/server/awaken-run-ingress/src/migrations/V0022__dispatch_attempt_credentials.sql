-- persist claim-epoch credential bindings and their fenced realization receipts
ALTER TABLE {prefix}_dispatch ADD COLUMN credential_bindings {json};
ALTER TABLE {prefix}_dispatch ADD COLUMN credential_receipts {json};
