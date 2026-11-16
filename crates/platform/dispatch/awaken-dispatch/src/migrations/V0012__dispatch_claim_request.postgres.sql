-- retain logical Worker claim requests
CREATE TABLE {prefix}_dispatch_claim_request (
    lease_owner TEXT NOT NULL,
    request_id TEXT NOT NULL,
    requested_run TEXT,
    run_id TEXT,
    lease_epoch BIGINT CHECK (lease_epoch > 0),
    claimed JSONB,
    expires_at_ms BIGINT NOT NULL,
    PRIMARY KEY (lease_owner, request_id)
);
CREATE INDEX {prefix}_dispatch_claim_request_expiry
    ON {prefix}_dispatch_claim_request (expires_at_ms);
CREATE INDEX {prefix}_dispatch_claim_request_run
    ON {prefix}_dispatch_claim_request (run_id) WHERE run_id IS NOT NULL;

-- Durable cutoff also fences purged requests after a database clock adjustment.
CREATE TABLE {prefix}_dispatch_claim_retention (
    singleton BOOLEAN PRIMARY KEY CHECK (singleton),
    expired_through_ms BIGINT NOT NULL
);
INSERT INTO {prefix}_dispatch_claim_retention VALUES (TRUE, 0);

ALTER TABLE {prefix}_dispatch
    ADD COLUMN claim_receipts_retired BOOLEAN NOT NULL DEFAULT FALSE;

CREATE FUNCTION {prefix}_retire_dispatch_claim_receipts() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_NARGS > 0 THEN
        -- Serialize Session deletion with the exact claim transaction, which
        -- also locks this Run. A racing or later claim cannot republish data.
        UPDATE {prefix}_dispatch SET claim_receipts_retired = TRUE
        WHERE run_id = OLD.run_id;
    END IF;
    UPDATE {prefix}_dispatch_claim_request
    SET requested_run = NULL, run_id = NULL, lease_epoch = NULL, claimed = NULL
    WHERE run_id = OLD.run_id;
    RETURN OLD;
END;
$$;

CREATE TRIGGER {prefix}_retire_claim_receipts_on_session_delete
    AFTER DELETE ON {prefix}_dispatch_session_membership
    FOR EACH ROW EXECUTE FUNCTION {prefix}_retire_dispatch_claim_receipts('session');
CREATE TRIGGER {prefix}_retire_claim_receipts_on_run_delete
    AFTER DELETE ON {prefix}_dispatch
    FOR EACH ROW EXECUTE FUNCTION {prefix}_retire_dispatch_claim_receipts();
