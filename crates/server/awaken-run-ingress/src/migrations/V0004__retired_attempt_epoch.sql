-- prevent delayed admission from resurrecting an acknowledged execution epoch
ALTER TABLE {prefix}_dispatch ADD COLUMN retired_attempt_epoch BIGINT NOT NULL DEFAULT 0
    CHECK (retired_attempt_epoch >= 0 AND retired_attempt_epoch <= lease_epoch
        AND (active_attempt_epoch IS NULL OR active_attempt_epoch > retired_attempt_epoch));
-- An old empty slot cannot distinguish a never-started claim from an ACK whose
-- response was lost. Retire that epoch conservatively; the next claim advances
-- it normally. Occupied historical slots remain blocked, never proved stopped.
UPDATE {prefix}_dispatch SET retired_attempt_epoch = lease_epoch
    WHERE active_attempt_owner IS NULL AND active_attempt_epoch IS NULL;
