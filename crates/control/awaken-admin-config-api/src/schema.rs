//! The admin-config schema (ADR-0043). One portable [`MigrationBundle`] under
//! the `admin` namespace with its own ledger, covering the aggregates the admin
//! plane itself authors (inference profiles, webhook endpoints, and resource
//! bindings) — the catalog/credential domains keep their own bundles. All rows
//! are **secret-free**. Its own
//! bundle prefix is what lets the admin plane be split into its own
//! database/service (blast-radius isolation).
//!
//! The DDL is NOT encoded in this source: every migration is a `.sql` file under
//! `migrations/`, embedded at build time with `include_str!`. The file name
//! carries the version (`V0005__…` ⇒ version 5) and the first `-- comment` line
//! is its description, so a schema change is a migration *file*, never a Rust
//! string literal.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the admin-config domain.
pub const BUNDLE_ID: &str = "awaken.admin";

/// Sole append point after the compact and expanded published histories reach
/// their terminal active schema.
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.admin.converged";

const EXPANDED_V1_CHECKSUM: &str =
    "a96581cecf572657f5503a0064d76b9b4c6653785d9ff74dd4821752735c604d";
const EXPANDED_V9_ALIAS: &str = "987ffe8ea131956d8b11c59ec8283d880ed97ce89cf02e9e652f61fbc4478131";

/// The embedded migration files, in apply order. Each entry is
/// `(file_name, file_contents)`: the name yields the version, the contents yield
/// the description (first `-- comment` line) and the SQL body. `include_str!`
/// resolves relative to this source file, so the `.sql` files ship in the crate.
const FILES: &[(&str, &str)] = &[(
    "V0001__control_admin.sql",
    include_str!("migrations/V0001__control_admin.sql"),
)];

const EXPANDED_FILES: &[(&str, &str)] = &[
    (
        "V0001__inference_profile.sql",
        include_str!("migrations/expanded/V0001__inference_profile.sql"),
    ),
    (
        "V0002__mcp_server.sql",
        include_str!("migrations/expanded/V0002__mcp_server.sql"),
    ),
    (
        "V0003__agent_mcp.sql",
        include_str!("migrations/expanded/V0003__agent_mcp.sql"),
    ),
    (
        "V0004__agent_resource.sql",
        include_str!("migrations/expanded/V0004__agent_resource.sql"),
    ),
    (
        "V0005__webhook.sql",
        include_str!("migrations/expanded/V0005__webhook.sql"),
    ),
    (
        "V0006__memory_store.sql",
        include_str!("migrations/expanded/V0006__memory_store.sql"),
    ),
    (
        "V0007__webhook_outbox.sql",
        include_str!("migrations/expanded/V0007__webhook_outbox.sql"),
    ),
    (
        "V0008__resource_catalog.sql",
        include_str!("migrations/expanded/V0008__resource_catalog.sql"),
    ),
    (
        "V0009__retire_legacy_mcp_config.sql",
        include_str!("migrations/expanded/V0009__retire_legacy_mcp_config.sql"),
    ),
    (
        "V0010__retire_legacy_webhook_outbox.sql",
        include_str!("migrations/expanded/V0010__retire_legacy_webhook_outbox.sql"),
    ),
    (
        "V0011__webhook_mutation_intent.sql",
        include_str!("migrations/expanded/V0011__webhook_mutation_intent.sql"),
    ),
];
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 11;

const EXPANDED_CHECKSUMS: &[&str] = &[
    EXPANDED_V1_CHECKSUM,
    "1b8affd53b8a63c79159f038054a6cbe8f8481750ab418197a86fa16c71f2361",
    "cf13ee409161606620bed4981d726f2bf901072fb08815b4ee538b70a3be3e19",
    "320d011eeb75a01dbbdb23c14c36bbad254bf900721af3ce290c921bcd4b130c",
    "d51e8efc131dcce7fadb91d703107a71ca0222bf30e37c2b46247abf86311b38",
    "4719cb5fc9f72f9045f0dc9ac85998c881ab2ae9a0fa76909da9fc7f80a9bd72",
    "e7b3c575526cea45205a0df343c392172b67f922e765a9a83c823743132205e7",
    "114ba1184c251e3a44ec0c1b85f4b48730147e1a0e2a768bc5d0f4cd5487728a",
    "43acd8cf624d2d939bff17aadb9c34061bc8f65207007b1c8dd68196a86763c4",
    "aaf91db3690bfe253ab45ab0291b161daab3bb973e005657d29528486e159763",
    "72b8a710c81abfc1f46630dcd792355bd2e7bdd39da4d24229df3121f12f74f5",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishedAdminStream {
    Compact,
    Expanded,
}

/// Parse the version from a `Vnnnn__slug.sql` file name (`V0005__…` ⇒ 5). A name
/// that does not carry a positive version yields `0`, which [`Migration::new`]
/// rejects — so a mis-named file fails the bundle build loudly.
fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

/// The migration's description: the first `-- comment` line of the file, so the
/// human-readable summary lives with the DDL rather than in this source.
fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|desc| !desc.is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Build the admin-schema migration bundle (prefix `admin`) from the embedded
/// `.sql` files.
pub fn admin_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            let version = version_of(name);
            let description = description_of(name, contents);
            Migration::new(version, description, contents.trim())
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

fn expanded_admin_bundle() -> Result<MigrationBundle, MigrationError> {
    assert_eq!(EXPANDED_FILES.len(), PUBLISHED_LEGACY_MIGRATION_COUNT);
    assert_eq!(EXPANDED_CHECKSUMS.len(), PUBLISHED_LEGACY_MIGRATION_COUNT);
    let migrations = EXPANDED_FILES
        .iter()
        .zip(EXPANDED_CHECKSUMS)
        .map(|((name, contents), checksum)| {
            let version = version_of(name);
            if version == 9 {
                Migration::published_legacy_with_aliases(
                    version,
                    description_of(name, contents),
                    contents.trim(),
                    *checksum,
                    [EXPANDED_V9_ALIAS],
                )
            } else {
                Migration::published_legacy(
                    version,
                    description_of(name, contents),
                    contents.trim(),
                    *checksum,
                )
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

fn published_admin_stream(v1_checksum: Option<&str>) -> PublishedAdminStream {
    if v1_checksum == Some(EXPANDED_V1_CHECKSUM) {
        PublishedAdminStream::Expanded
    } else {
        PublishedAdminStream::Compact
    }
}

pub(crate) fn selected_admin_bundle(
    v1_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    match published_admin_stream(v1_checksum) {
        PublishedAdminStream::Compact => admin_bundle(),
        PublishedAdminStream::Expanded => expanded_admin_bundle(),
    }
}

pub(crate) fn converged_admin_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged admin migration history",
            "SELECT 1",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_bundle_lints() {
        // Causes: C1 the current Control Admin schema is one registered baseline;
        // C2 it uses the ordinary deterministic constructor. Effects: E1 lint
        // succeeds and an empty ledger applies exactly once; E2 schema/checksum
        // drift fails closed. Rule A1=C1+C2=>E1; the common migration runner owns
        // replay and E2.
        for bundle in [
            admin_bundle().expect("compact bundle"),
            expanded_admin_bundle().expect("expanded bundle"),
            converged_admin_bundle().expect("converged bundle"),
        ] {
            awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
        }
    }

    #[test]
    fn versions_parse_contiguously_from_file_names() {
        // Decision table: A1 empty ledger + the current baseline -> V1; A2 the
        // exact V1 receipt -> no pending SQL; A3 an old/mutated receipt -> fail
        // closed. This assertion owns A1's one-version input; shared runner tests
        // own A2/A3.
        let bundle = admin_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1]);
    }

    #[test]
    fn published_admin_histories_select_exact_receipts() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, MigrationError, plan};

        /* History decision table: H1 empty/compact V1 -> compact baseline;
         * H2 expanded V1 -> exact V1..V11 and no active-table rewrite;
         * H3 unknown V1 -> compact bundle reports checksum mismatch; H4 either
         * terminal history -> only the shared convergence receipt can append.
         */
        let compact = admin_bundle().expect("compact");
        let expanded = expanded_admin_bundle().expect("expanded");
        let compact_v1 = compact.migrations()[0].checksum_for(Dialect::Sqlite);
        assert_eq!(published_admin_stream(None), PublishedAdminStream::Compact);
        assert_eq!(
            published_admin_stream(Some(&compact_v1)),
            PublishedAdminStream::Compact
        );
        assert_eq!(
            published_admin_stream(Some(EXPANDED_V1_CHECKSUM)),
            PublishedAdminStream::Expanded
        );
        for (index, expected) in EXPANDED_CHECKSUMS.iter().enumerate() {
            assert_eq!(
                expanded.migrations()[index].checksum_for(Dialect::Sqlite),
                *expected,
                "H2 V{}",
                index + 1
            );
        }
        let live_v9 = BTreeMap::from([(9, EXPANDED_V9_ALIAS.to_owned())]);
        assert!(
            plan(&expanded, &live_v9, Dialect::Sqlite)
                .expect("H2 live V9 alias")
                .iter()
                .all(|migration| migration.version() != 9)
        );
        let unknown = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_admin_bundle(Some(&"f".repeat(64))).expect("H3 select"),
                &unknown,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
    }
}
