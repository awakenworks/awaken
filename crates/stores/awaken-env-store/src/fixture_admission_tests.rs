//! Execute each live entry's missing/invalid/live table through its real binary.

#[test]
fn file_and_postgres_entry_points_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::file_open_and_pg_connect_entry_points",
    );
}

#[test]
fn postgres_parity_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::postgres_parity_over_a_live_db",
    );
}
