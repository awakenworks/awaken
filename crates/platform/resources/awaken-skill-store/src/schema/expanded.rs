//! Exact V1..V2 Skill Store history published before the aggregate baseline.
//! The retired V1 projection has no repository adapter and remains inert.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

use super::BUNDLE_ID;

pub(super) const V1_CHECKSUM: &str =
    "c28a0984094d64b4ac74cfc1e9226c5c7b713729728add48ada994394a09b99c";
pub(super) const V2_CHECKSUM: &str =
    "e067f788ff29a14297083d309fd2e0d96343f2c8a36c46f52c1cbcfad77a19e2";
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 2;
const V1: &str = include_str!("../migrations/expanded/V0001__skill.sql");
const V2: &str = include_str!("../migrations/expanded/V0002__aggregate.sql");

pub(super) fn bundle() -> Result<MigrationBundle, MigrationError> {
    assert_eq!(PUBLISHED_LEGACY_MIGRATION_COUNT, 2);
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::published_legacy(
                1,
                "delivered skills: SKILL.md bodies, keyed by (workspace, id)",
                V1.trim(),
                V1_CHECKSUM,
            )?,
            Migration::published_legacy(
                2,
                "complete binary-safe Skill aggregate; replaces the V1 current-SKILL.md projection",
                V2.trim(),
                V2_CHECKSUM,
            )?,
        ],
    )
}
