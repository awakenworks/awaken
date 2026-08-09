use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const COORDINATOR_CAPTURE_BUNDLE_ID: &str = "awaken.coordinator_data_capture";
pub const COORDINATOR_CAPTURE_PREFIX: &str = "coordinator_data_capture";

pub fn coordinator_data_capture_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        COORDINATOR_CAPTURE_BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "Coordinator-owned subject-tagged captured runtime content",
                include_str!("migrations/V0001__coordinator_data_capture.sql").trim(),
            )?,
            Migration::new(
                2,
                "Coordinator capture fence for erased data subjects",
                include_str!("migrations/V0002__capture_fence.sql").trim(),
            )?,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_capture_bundle_is_independent_and_lint_clean() {
        // Cause/effect: both ordered Coordinator capture files produce one bundle
        // and one prefix; deterministic migration lint rejects unsafe DDL.
        let bundle = coordinator_data_capture_bundle().unwrap();
        assert_eq!(bundle.bundle_id(), COORDINATOR_CAPTURE_BUNDLE_ID);
        awaken_scoped_migration::lint(&[bundle]).unwrap();
    }
}
