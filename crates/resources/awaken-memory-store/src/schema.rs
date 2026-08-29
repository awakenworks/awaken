//! The memory-store schema. Fresh databases use the compact path-addressed
//! history. Databases that already recorded the older blob-first history keep
//! that immutable receipt stream and append the same active schema effects.
//! Both histories then converge on one future migration bundle; the retired
//! blob table has no repository or runtime writer.
//!
//! The DDL is a `.sql` file under `migrations/`, embedded with `include_str!`: the
//! file name carries the version (`V0001__…` ⇒ version 1) and the first
//! `-- comment` line is its description.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the memory-store domain.
pub const BUNDLE_ID: &str = "awaken.memory_store";

/// Sole append point after either published numbering history reaches its
/// terminal active schema.
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.memory_store.converged";

const EXPANDED_V1_SQL: &str = include_str!("migrations/expanded/V0001__blob.sql");
const EXPANDED_V1_CHECKSUM: &str =
    "81b2243920d5de36514457f78cff4983276e840c911d146be797a0e0e47a4a17";
#[cfg(test)]
const EXPANDED_V2_CHECKSUM: &str =
    "daee28b1947f5ee9082b3f9cbd8664c0bb8ded25184a75af0c8fde2c193b313d";
#[cfg(test)]
const EXPANDED_V3_CHECKSUM: &str =
    "d05a7f3db8c30e460502a5bb5660bfc3f1c8e00ff5c836a86e61aea2a9fbb508";
#[cfg(test)]
const COMPACT_V1_CHECKSUM: &str =
    "8bf1ca591376cb124c299b64f3488653d73272555fd841a612c43ab64addeeed";

/// Embedded migration files, in apply order (`(name, contents)`).
const FILES: &[(&str, &str)] = &[
    (
        "V0001__memories.sql",
        include_str!("migrations/V0001__memories.sql"),
    ),
    (
        "V0002__versions_and_counters.sql",
        include_str!("migrations/V0002__versions_and_counters.sql"),
    ),
    (
        "V0003__version_actors.sql",
        include_str!("migrations/V0003__version_actors.sql"),
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishedMemoryStream {
    Compact,
    Expanded,
}

/// Version from a `Vnnnn__slug.sql` file name (`V0001__…` ⇒ 1); a non-positive
/// value is rejected by [`Migration::new`], so a mis-named file fails loudly.
fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

/// The first `-- comment` line of the file — the description lives with the DDL.
fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|desc| !desc.is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Build the memory-store migration bundle (prefix `memory_store`).
pub fn memory_store_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

fn expanded_memory_store_bundle() -> Result<MigrationBundle, MigrationError> {
    let mut migrations = vec![Migration::published_legacy(
        1,
        description_of("V0001__blob.sql", EXPANDED_V1_SQL),
        EXPANDED_V1_SQL.trim(),
        EXPANDED_V1_CHECKSUM,
    )?];
    migrations.extend(
        FILES
            .iter()
            .enumerate()
            .map(|(index, (name, contents))| {
                Migration::new(
                    i64::try_from(index).expect("three memory migrations") + 2,
                    description_of(name, contents),
                    contents.trim(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    MigrationBundle::new(BUNDLE_ID, migrations)
}

fn published_memory_stream(v1_checksum: Option<&str>) -> PublishedMemoryStream {
    if v1_checksum == Some(EXPANDED_V1_CHECKSUM) {
        PublishedMemoryStream::Expanded
    } else {
        PublishedMemoryStream::Compact
    }
}

/// Select an immutable published history using only its V1 receipt. Unknown
/// receipts deliberately select the compact stream so the ordinary migration
/// runner reports a checksum mismatch and performs no mutation.
pub(crate) fn selected_memory_store_bundle(
    v1_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    match published_memory_stream(v1_checksum) {
        PublishedMemoryStream::Compact => memory_store_bundle(),
        PublishedMemoryStream::Expanded => expanded_memory_store_bundle(),
    }
}

pub(crate) fn converged_memory_store_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged memory-store migration history",
            "SELECT 1",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_bundle_lints() {
        // Causes: C1 compact and expanded histories retain exact immutable SQL;
        // C2 the convergence bundle is the only future append point. Effects:
        // E1 all three bundles lint; E2 altered/conditional/duplicate SQL fails
        // before storage. Rules L1=C1+C2=>E1; L2=!C1|!C2=>E2.
        for bundle in [
            memory_store_bundle().expect("compact bundle"),
            expanded_memory_store_bundle().expect("expanded bundle"),
            converged_memory_store_bundle().expect("converged bundle"),
        ] {
            awaken_scoped_migration::lint(std::slice::from_ref(&bundle))
                .expect("memory-store bundle lints");
        }
    }

    #[test]
    fn published_histories_select_and_complete_exact_receipts() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, MigrationError, plan};

        /* Published-history decision table:
         * H1 no V1 -> compact V1..V3; H2 compact V1 -> compact suffix;
         * H3 expanded V1 -> expanded V1..V4; H4 unknown V1 -> ordinary
         * checksum mismatch/no mutation. Both H2/H3 append only convergence.
         */
        let compact = memory_store_bundle().expect("compact");
        let expanded = expanded_memory_store_bundle().expect("expanded");
        assert_eq!(
            compact.migrations()[0].checksum_for(Dialect::Sqlite),
            COMPACT_V1_CHECKSUM,
            "H1/H2 compact receipt"
        );
        assert_eq!(
            expanded.migrations()[0].checksum_for(Dialect::Sqlite),
            EXPANDED_V1_CHECKSUM,
            "H3 expanded V1"
        );
        assert_eq!(
            expanded.migrations()[1].checksum_for(Dialect::Sqlite),
            EXPANDED_V2_CHECKSUM,
            "H3 expanded V2"
        );
        assert_eq!(
            expanded.migrations()[2].checksum_for(Dialect::Sqlite),
            EXPANDED_V3_CHECKSUM,
            "H3 expanded V3"
        );
        assert_eq!(
            published_memory_stream(None),
            PublishedMemoryStream::Compact
        );
        assert_eq!(
            published_memory_stream(Some(COMPACT_V1_CHECKSUM)),
            PublishedMemoryStream::Compact
        );
        assert_eq!(
            published_memory_stream(Some(EXPANDED_V1_CHECKSUM)),
            PublishedMemoryStream::Expanded
        );
        for bundle in [&compact, &expanded] {
            for prefix in 0..=bundle.migrations().len() {
                let applied = bundle
                    .migrations()
                    .iter()
                    .take(prefix)
                    .map(|migration| (migration.version(), migration.checksum_for(Dialect::Sqlite)))
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(
                    plan(bundle, &applied, Dialect::Sqlite)
                        .expect("published prefix completes")
                        .len(),
                    bundle.migrations().len() - prefix,
                    "H1-H3 prefix {prefix}"
                );
            }
        }
        let unknown = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(
            matches!(
                plan(
                    &selected_memory_store_bundle(Some(&"f".repeat(64))).expect("H4 select"),
                    &unknown,
                    Dialect::Sqlite,
                ),
                Err(MigrationError::ChecksumMismatch { version: 1, .. })
            ),
            "H4"
        );
    }

    #[test]
    fn expanded_sqlite_history_adds_actor_columns_and_reopens() {
        use awaken_scoped_migration::MigrationBundle;
        use rusqlite::Connection;

        /* Storage decision table: S1 exact expanded V1..V3 -> append V4 actor
         * columns and convergence without touching rows; S2 terminal history ->
         * zero writes; S3 malformed/drifted receipt -> runner rejects atomically.
         */
        let connection = Connection::open_in_memory().expect("sqlite");
        let expanded = expanded_memory_store_bundle().expect("expanded");
        let published = MigrationBundle::new(BUNDLE_ID, expanded.migrations()[..3].to_vec())
            .expect("published V1..V3");
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix("memory_store")
                .expect("runner");
        runner
            .run_bundle(&connection, &published)
            .expect("seed published history");
        connection
            .execute(
                "INSERT INTO memory_store_versions
                 (store_id, ordinal, id, memory_id, operation, path, content, created)
                 VALUES ('s', 1, 'v1', 'm1', 'created', '/a.md', X'61', 1)",
                [],
            )
            .expect("seed version");
        assert_eq!(
            runner
                .run_bundle(&connection, &expanded)
                .expect("S1 append V4")
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [4],
            "S1"
        );
        runner
            .run_bundle(
                &connection,
                &converged_memory_store_bundle().expect("converged"),
            )
            .expect("S1 convergence");
        let columns = connection
            .prepare("PRAGMA table_info(memory_store_versions)")
            .expect("columns")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("column values");
        assert!(columns.iter().any(|column| column == "created_by_json"));
        assert!(columns.iter().any(|column| column == "redacted_by_json"));
        assert!(
            runner
                .run_bundle(&connection, &expanded)
                .expect("S2 reopen")
                .is_empty(),
            "S2"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM memory_store_versions", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("preserved row"),
            1,
            "S1 preserves data"
        );
    }
}
