-- transactional dense commit order replaces the rollback-gapped sequence
--
-- The commit table is the one global history authority. Compact any gaps left
-- by the retired non-transactional sequence before dropping it. Ascending
-- updates make each smaller dense key vacant before the following row moves;
-- dependent projections and receipts move in the same migration transaction.
LOCK TABLE {prefix}_commit, {prefix}_message, {prefix}_state_command,
    {prefix}_event, {prefix}_commit_receipt IN SHARE ROW EXCLUSIVE MODE;

DO $awaken_dense_commit_order$
DECLARE
    mapping RECORD;
BEGIN
    FOR mapping IN
        SELECT sequence AS old_sequence,
               ROW_NUMBER() OVER (ORDER BY sequence) AS dense_sequence
        FROM {prefix}_commit
        ORDER BY sequence
    LOOP
        IF mapping.old_sequence <> mapping.dense_sequence THEN
            UPDATE {prefix}_message
               SET commit_sequence = mapping.dense_sequence
             WHERE commit_sequence = mapping.old_sequence;
            UPDATE {prefix}_state_command
               SET commit_sequence = mapping.dense_sequence
             WHERE commit_sequence = mapping.old_sequence;
            UPDATE {prefix}_commit_receipt
               SET commit_sequence = mapping.dense_sequence
             WHERE commit_sequence = mapping.old_sequence;
            UPDATE {prefix}_event
               SET sequence = mapping.dense_sequence * 1000 + MOD(sequence, 1000)
             WHERE sequence / 1000 = mapping.old_sequence;
            UPDATE {prefix}_commit
               SET sequence = mapping.dense_sequence
             WHERE sequence = mapping.old_sequence;
        END IF;
    END LOOP;
END
$awaken_dense_commit_order$;

DROP SEQUENCE {prefix}_commit_seq;
