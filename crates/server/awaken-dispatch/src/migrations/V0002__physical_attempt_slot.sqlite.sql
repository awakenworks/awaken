-- retain one exact physical executor until it acknowledges quiescence
ALTER TABLE {prefix}_dispatch ADD COLUMN active_attempt_owner TEXT;
ALTER TABLE {prefix}_dispatch ADD COLUMN active_attempt_epoch BIGINT;

CREATE TRIGGER {prefix}_dispatch_attempt_slot_pair_insert
BEFORE INSERT ON {prefix}_dispatch
WHEN (NEW.active_attempt_owner IS NULL) <> (NEW.active_attempt_epoch IS NULL)
    OR NEW.active_attempt_epoch < 0
BEGIN
    SELECT RAISE(ABORT, 'dispatch physical attempt slot must be a non-negative owner/epoch pair');
END;

CREATE TRIGGER {prefix}_dispatch_attempt_slot_pair_update
BEFORE UPDATE OF active_attempt_owner, active_attempt_epoch ON {prefix}_dispatch
WHEN (NEW.active_attempt_owner IS NULL) <> (NEW.active_attempt_epoch IS NULL)
    OR NEW.active_attempt_epoch < 0
BEGIN
    SELECT RAISE(ABORT, 'dispatch physical attempt slot must be a non-negative owner/epoch pair');
END;
