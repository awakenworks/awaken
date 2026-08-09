//! Versioned schema for the Control-owned data-subject store.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const CONTROL_BUNDLE_ID: &str = "awaken.control_data_subject";
pub const CONTROL_PREFIX: &str = "control_data_subject";

const CONTROL_FILES: &[(&str, &str)] = &[
    (
        "V0001__control_data_subject.sql",
        include_str!("migrations/V0001__control_data_subject.sql"),
    ),
    (
        "V0002__control_erasure_job.sql",
        include_str!("migrations/V0002__control_erasure_job.sql"),
    ),
    (
        "V0003__subject_revision.sql",
        include_str!("migrations/V0003__subject_revision.sql"),
    ),
    (
        "V0004__erasure_job_revision.sql",
        include_str!("migrations/V0004__erasure_job_revision.sql"),
    ),
];

// The V1/V2 DDL was already published before the store/application ownership
// refactor. That refactor edited only the leading SQL comments, producing a
// second receipt identity for the same terminal schema. Keep the original body
// canonical and accept the two exact edited-comment receipts without ever
// executing an alternate migration body.
const EDITED_COMMENT_ALIASES: &[(i64, &str, &str)] = &[
    (
        1,
        "f76eb76d429e5deb60489bfae2eabbd2443ec31644c29602c5367a7770619661",
        "4632a05dbb5f9865a3698f37797baebd8f422d8e93c39ed06803c089baea3372",
    ),
    (
        2,
        "e02667cc2334d2347d7c981acec4f7d3be5194877365e70798383bf7a391cde5",
        "3e7d56edc091d30f9e8dff22c7bce1a2e0d75c9b8097e007052e6d015c4c3afa",
    ),
];

fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_owned()))
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.to_owned())
}

fn bundle(
    bundle_id: &'static str,
    files: &[(&str, &str)],
) -> Result<MigrationBundle, MigrationError> {
    let migrations = files
        .iter()
        .map(|(name, contents)| {
            let version = version_of(name);
            let description = description_of(name, contents);
            let sql = contents.trim();
            if let Some((_, expected, alias)) = EDITED_COMMENT_ALIASES
                .iter()
                .find(|(published_version, _, _)| *published_version == version)
            {
                Migration::published_legacy_with_aliases(
                    version,
                    description,
                    sql,
                    *expected,
                    [*alias],
                )
            } else {
                Migration::new(version, description, sql)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(bundle_id, migrations)
}

pub fn control_data_subject_bundle() -> Result<MigrationBundle, MigrationError> {
    bundle(CONTROL_BUNDLE_ID, CONTROL_FILES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bundle_is_lint_clean() {
        // Cause/effect: the Control subject/erasure files produce one bundle and
        // one prefix; deterministic migration lint rejects unsafe DDL.
        let control = control_data_subject_bundle().expect("Control bundle builds");
        assert_eq!(control.bundle_id(), CONTROL_BUNDLE_ID);
        awaken_scoped_migration::lint(&[control]).expect("lint");
    }

    #[test]
    fn published_comment_receipts_converge_without_replaying_schema() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, MigrationError, plan};

        /* Published-comment cause/effect decision table. Causes: C1 an empty
         * ledger; C2 the original V1/V2 receipts; C3 the ownership-refactor
         * V1/V2 receipts whose bodies differ only in comments; C4 an unknown
         * receipt. Effects: E1 execute only the original canonical V1..V4 SQL;
         * E2 continue both recognized histories with V3/V4 and never replay
         * V1/V2; E3 fail closed. Rules: H1 C1=>E1; H2 C2|C3=>E2;
         * H3 C4=>E3. Constraint: aliases verify existing receipts only; the
         * original body remains the sole SQL executed for a missing version. */
        let bundle = control_data_subject_bundle().expect("bundle builds");
        assert_eq!(
            plan(&bundle, &BTreeMap::new(), Dialect::Sqlite)
                .expect("H1 empty ledger")
                .iter()
                .map(|migration| migration.version())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );

        let canonical = EDITED_COMMENT_ALIASES
            .iter()
            .map(|(version, expected, _)| (*version, (*expected).to_string()))
            .collect::<BTreeMap<_, _>>();
        let edited = EDITED_COMMENT_ALIASES
            .iter()
            .map(|(version, _, alias)| (*version, (*alias).to_string()))
            .collect::<BTreeMap<_, _>>();
        for (rule, receipts) in [("H2 canonical", canonical), ("H2 edited", edited)] {
            assert_eq!(
                plan(&bundle, &receipts, Dialect::Sqlite)
                    .unwrap_or_else(|error| panic!("{rule}: {error}"))
                    .iter()
                    .map(|migration| migration.version())
                    .collect::<Vec<_>>(),
                [3, 4],
                "{rule}"
            );
        }

        let unknown = BTreeMap::from([(1, "unknown-receipt".to_string())]);
        assert!(matches!(
            plan(&bundle, &unknown, Dialect::Sqlite).unwrap_err(),
            MigrationError::ChecksumMismatch { version: 1, .. }
        ));
    }
}
