-- reject negative durable authority counters at the storage boundary
CREATE TRIGGER {prefix}_dispatch_nonnegative_authority_insert
BEFORE INSERT ON {prefix}_dispatch
WHEN NEW.lease_epoch < 0 OR NEW.attempt_count < 0 OR NEW.epoch < 0
BEGIN
    SELECT RAISE(ABORT, 'dispatch authority counters must be non-negative');
END;

CREATE TRIGGER {prefix}_dispatch_nonnegative_authority_update
BEFORE UPDATE OF lease_epoch, attempt_count, epoch ON {prefix}_dispatch
WHEN NEW.lease_epoch < 0 OR NEW.attempt_count < 0 OR NEW.epoch < 0
BEGIN
    SELECT RAISE(ABORT, 'dispatch authority counters must be non-negative');
END;

CREATE TRIGGER {prefix}_pending_nonnegative_revision_insert
BEFORE INSERT ON {prefix}_pending
WHEN NEW.revision < 0
BEGIN
    SELECT RAISE(ABORT, 'pending revision must be non-negative');
END;

CREATE TRIGGER {prefix}_pending_nonnegative_revision_update
BEFORE UPDATE OF revision ON {prefix}_pending
WHEN NEW.revision < 0
BEGIN
    SELECT RAISE(ABORT, 'pending revision must be non-negative');
END;

-- Force every existing row through the same predicates. The migration runner
-- wraps this body in a transaction, so corrupt legacy data rolls the triggers
-- and receipt back together instead of leaving a partially upgraded schema.
UPDATE {prefix}_dispatch
SET lease_epoch = lease_epoch, attempt_count = attempt_count, epoch = epoch;

UPDATE {prefix}_pending SET revision = revision;
