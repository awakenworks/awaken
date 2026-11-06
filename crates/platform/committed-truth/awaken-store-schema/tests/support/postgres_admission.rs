//! Each PostgreSQL migration entry must enforce the same fixture admission.

#[test]
fn postgres_fixture_admission() {
    for case in [
        "bundle_applies_on_real_postgres",
        "every_postgres_historical_prefix_migrates_to_full",
        "concurrent_postgres_startup_has_one_migration_applier",
        "postgres_failed_migration_rolls_back_schema_and_ledger_rows",
        "postgres_v10_canonicalizes_historical_kinds_and_rolls_back_as_one_unit",
        "postgres_v11_indexes_run_predecessors_without_rewriting_history",
        "postgres_v12_indexes_recovery_ranges_atomically_without_changing_rows",
    ] {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(case);
    }
}
