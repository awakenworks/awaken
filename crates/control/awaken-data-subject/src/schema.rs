//! Versioned schemas for the two bounded-context responsibilities exposed by
//! this crate. Control owns subject consent and erasure orchestration;
//! Coordinator owns captured runtime content. Each responsibility has an
//! independent ledger and table prefix even when AllInOne shares one database.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const CONTROL_BUNDLE_ID: &str = "awaken.control_data_subject";
pub const COORDINATOR_CAPTURE_BUNDLE_ID: &str = "awaken.coordinator_data_capture";
pub const CONTROL_PREFIX: &str = "control_data_subject";
pub const COORDINATOR_CAPTURE_PREFIX: &str = "coordinator_data_capture";

const CONTROL_FILES: &[(&str, &str)] = &[
    (
        "V0001__control_data_subject.sql",
        include_str!("migrations/V0001__control_data_subject.sql"),
    ),
    (
        "V0002__control_erasure_job.sql",
        include_str!("migrations/V0002__control_erasure_job.sql"),
    ),
];

const COORDINATOR_CAPTURE_FILES: &[(&str, &str)] = &[(
    "V0001__coordinator_data_capture.sql",
    include_str!("migrations/V0001__coordinator_data_capture.sql"),
)];

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
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(bundle_id, migrations)
}

pub fn control_data_subject_bundle() -> Result<MigrationBundle, MigrationError> {
    bundle(CONTROL_BUNDLE_ID, CONTROL_FILES)
}

pub fn coordinator_data_capture_bundle() -> Result<MigrationBundle, MigrationError> {
    bundle(COORDINATOR_CAPTURE_BUNDLE_ID, COORDINATOR_CAPTURE_FILES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_context_bundles_are_independent_and_lint_clean() {
        // Cause/effect decision table:
        // R1 Control subject/erasure DDL -> only the Control bundle/prefix.
        // R2 Coordinator captured-content DDL -> only its bundle/prefix.
        // R3 both bundles composed by AllInOne -> independent ledgers and no
        // duplicate table ownership; the common linter accepts both together.
        let control = control_data_subject_bundle().expect("Control bundle builds");
        let capture = coordinator_data_capture_bundle().expect("Coordinator bundle builds");
        assert_eq!(control.bundle_id(), CONTROL_BUNDLE_ID, "R1");
        assert_eq!(capture.bundle_id(), COORDINATOR_CAPTURE_BUNDLE_ID, "R2");
        awaken_scoped_migration::lint(&[control, capture]).expect("R3");
    }
}
