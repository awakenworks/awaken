-- Bind a run to the sandbox it was placed on (B-P3, ADR-0021 §6). Opaque to the
-- dispatch aggregate (the fleet serializes a SandboxHandle into it); durable so
-- claim returns it on recovery and reconcile_adoption can re-adopt the sandbox.
ALTER TABLE {prefix}_dispatch ADD COLUMN sandbox TEXT;
