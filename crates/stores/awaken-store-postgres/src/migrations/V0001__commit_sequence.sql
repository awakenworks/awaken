-- commit sequence: lock-free monotonic allocation for the commit PK
--
-- The commit PK (`{prefix}_commit.sequence`) must be allocated race-free across a
-- multi-node fleet AND across parallel drives in one process. The portable schema
-- keeps `sequence` a plain BIGINT so it renders for SQLite too; on Postgres a
-- dedicated SEQUENCE lets `nextval` allocate the next value with a short internal
-- latch (never held to transaction end), so concurrent committers do not convoy on
-- one lock across the whole commit fsync. Gaps (a rolled-back allocation) are the
-- accepted trade: the commit log's contract is strict monotonicity, not contiguity.
--
-- This is a Postgres-only optimisation, so it lives in the Postgres backend crate as
-- its own scoped bundle rather than the portable commit schema. It runs once,
-- under the migrator's per-bundle applier guard, so the fleet never races the
-- create/seed and a late-joining node never rewinds the counter. Seeding jumps the
-- sequence past any pre-existing `sequence` so an upgraded, already-populated
-- database continues without colliding on the PK.
CREATE SEQUENCE IF NOT EXISTS {prefix}_commit_seq;

SELECT setval(
    '{prefix}_commit_seq',
    GREATEST((SELECT COALESCE(MAX(sequence), 0) FROM {prefix}_commit), 1),
    (SELECT COUNT(*) > 0 FROM {prefix}_commit)
);
