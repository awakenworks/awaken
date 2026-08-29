//! Exact V1..V3 Sandbox Policy history published before the compact baseline.
//! The legacy environment-binding table is never read or written here; it is
//! retained only so existing ledgers can be verified without inventing a
//! second owner beside `EnvRegistry`.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

use super::BUNDLE_ID;

pub(super) const V1_CHECKSUM: &str =
    "8a7c8879b0530755672ef2a4e9912331c7acfb9071e81a07e4b2722773c637e4";
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 3;
const V1: &str = include_str!("migrations/expanded/V0001__policy_version.sql");
const V2: &str = include_str!("migrations/expanded/V0002__current_policy.sql");
const V3: &str = include_str!("migrations/expanded/V0003__environment_binding.sql");

pub(super) fn bundle() -> Result<MigrationBundle, MigrationError> {
    assert_eq!(PUBLISHED_LEGACY_MIGRATION_COUNT, 3);
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::published_legacy(
                1,
                "immutable sandbox execution policy versions",
                V1.trim(),
                V1_CHECKSUM,
            )?,
            Migration::published_legacy(
                2,
                "current sandbox execution policy version",
                V2.trim(),
                "9945afe967af15a5a43dd52c57782524dfab0bc0408bbc3efef88f18e4926049",
            )?,
            Migration::published_legacy(
                3,
                "exact environment sandbox execution policy binding",
                V3.trim(),
                "9d94a82c5084ece33984423bd646e8f9d08de8b78c9067a2c7598a0b3a7b48d1",
            )?,
        ],
    )
}
