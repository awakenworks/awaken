//! Admission of an explicitly configured PostgreSQL verification fixture. The
//! existing database runner provisions one shared database; every admission
//! owns a fresh namespace in it, so no test observes another test's rows. Each
//! domain still owns its tables and laws inside that namespace.

use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// One explicit configuration case for a backend test run in a child process.
enum PostgresFixtureChild<'a> {
    OptionalMissing,
    RequiredMissing,
    RequiredUrl(&'a str),
}

/// Run an exact backend test with fixture inputs isolated from the parent gate.
/// Each domain still owns its schema and its live backend behavior.
fn postgres_fixture_child_output(
    test_name: &str,
    fixture: PostgresFixtureChild<'_>,
) -> std::io::Result<Output> {
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(["--exact", test_name, "--nocapture"])
        .env_remove("AWAKEN_TEST_DATABASE_URL")
        .env_remove("AWAKEN_TEST_REQUIRE_POSTGRES");
    match fixture {
        PostgresFixtureChild::OptionalMissing => {}
        PostgresFixtureChild::RequiredMissing => {
            child.env("AWAKEN_TEST_REQUIRE_POSTGRES", "1");
        }
        PostgresFixtureChild::RequiredUrl(url) => {
            child
                .env("AWAKEN_TEST_REQUIRE_POSTGRES", "1")
                .env("AWAKEN_TEST_DATABASE_URL", url);
        }
    }
    child.output()
}

/// Exercise the one fixture-admission decision table through a real backend
/// test process. The full PostgreSQL gate supplies a live URL; local runs can
/// still verify the negative cases without an external database.
pub fn assert_postgres_fixture_child_contract(test_name: &str) {
    let run = |fixture| {
        postgres_fixture_child_output(test_name, fixture).expect("PostgreSQL fixture rule child")
    };
    let optional = run(PostgresFixtureChild::OptionalMissing);
    assert!(optional.status.success(), "optional missing fixture");
    assert!(
        String::from_utf8_lossy(&optional.stderr).contains("SKIPPED PostgreSQL fixture"),
        "optional missing fixture must report its skip"
    );

    let required = run(PostgresFixtureChild::RequiredMissing);
    assert!(!required.status.success(), "required missing fixture");
    assert!(
        String::from_utf8_lossy(&required.stderr)
            .contains("required PostgreSQL fixture AWAKEN_TEST_DATABASE_URL is not configured"),
        "required missing fixture must fail for the expected cause"
    );

    for url in ["", "postgres://127.0.0.1:1/unreachable"] {
        let invalid = run(PostgresFixtureChild::RequiredUrl(url));
        assert!(!invalid.status.success(), "configured invalid fixture");
        assert!(
            !String::from_utf8_lossy(&invalid.stderr).contains("SKIPPED PostgreSQL fixture"),
            "configured invalid fixture must not become a skip"
        );
    }

    if let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") {
        let live = run(PostgresFixtureChild::RequiredUrl(&url));
        assert!(live.status.success(), "configured live fixture");
        assert!(
            !String::from_utf8_lossy(&live.stderr).contains("SKIPPED PostgreSQL fixture"),
            "configured live fixture must execute the backend"
        );
    }
}

/// No configuration is an explicit local skip. Invalid configuration is not
/// absence. Once Some is returned, callers must require connection/setup to
/// succeed rather than erase either failure into a skipped test.
///
/// The returned URL selects a fresh schema created for this admission alone
/// (`search_path` in the connection options). Recovery or archive scans that
/// read "every row" therefore see only the rows of the admitting test, whether
/// other tests share the database concurrently or ran before it. Callers that
/// need a second shared client pass this exact URL on; calling again admits a
/// new, disjoint namespace. A caller may still select its own schema, since a
/// later `search_path` option or `SET` replaces this one.
pub fn postgres_fixture_url() -> Option<String> {
    let required = postgres_fixture_required(std::env::var("AWAKEN_TEST_REQUIRE_POSTGRES"));
    let configured = admit_postgres_fixture(std::env::var("AWAKEN_TEST_DATABASE_URL"), required);
    let Some(url) = configured else {
        // Direct stderr stays visible even when libtest captures successful
        // tests' print macros. A local skip must not look like backend evidence.
        use std::io::Write as _;
        writeln!(
            std::io::stderr(),
            "SKIPPED PostgreSQL fixture: AWAKEN_TEST_DATABASE_URL is not configured"
        )
        .expect("report PostgreSQL fixture admission");
        return None;
    };
    let namespace = fresh_namespace();
    create_namespace(&url, &namespace);
    Some(namespaced_url(&url, &namespace))
}

/// Connect an admitted PostgreSQL fixture URL. Connection failure rejects the
/// verification run; domain laws remain with the caller.
pub async fn postgres_fixture_pool(url: &str) -> sqlx::PgPool {
    awaken_sql_storage::postgres_pool_options()
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .expect("configured PostgreSQL fixture must connect")
}

/// A schema name unique across concurrent test processes and repeated runs on
/// one database: process id, wall-clock nanoseconds and a process counter.
/// Only `[a-z0-9_]` and at most 63 bytes, so it needs no quoting.
fn fresh_namespace() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "awaken_fixture_{:x}_{:x}_{:x}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Create the namespace on a private runtime thread, so admission works the
/// same from synchronous tests and from inside any async test runtime. A
/// configured fixture that cannot connect or create the schema fails the test.
fn create_namespace(url: &str, namespace: &str) {
    let url = url.to_owned();
    let statement = format!("CREATE SCHEMA {namespace}");
    let created = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("PostgreSQL fixture namespace runtime")
            .block_on(async move {
                let admin = postgres_fixture_pool(&url).await;
                sqlx::query(&statement)
                    .execute(&admin)
                    .await
                    .expect("configured PostgreSQL fixture must create its namespace");
                admin.close().await;
            });
    })
    .join();
    if let Err(panic) = created {
        std::panic::resume_unwind(panic);
    }
}

/// Append the namespace as a libpq/sqlx `options` parameter, keeping every
/// parameter the runner configured.
fn namespaced_url(url: &str, namespace: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-c%20search_path%3D{namespace}")
}

fn postgres_fixture_required(value: Result<String, std::env::VarError>) -> bool {
    match value {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        _ => panic!("AWAKEN_TEST_REQUIRE_POSTGRES must be 1 or unset"),
    }
}

fn admit_postgres_fixture(
    value: Result<String, std::env::VarError>,
    required: bool,
) -> Option<String> {
    match value {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) if !required => None,
        Err(std::env::VarError::NotPresent) => {
            panic!("required PostgreSQL fixture AWAKEN_TEST_DATABASE_URL is not configured")
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("AWAKEN_TEST_DATABASE_URL must be valid UTF-8")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_admission_distinguishes_absent_configured_and_invalid() {
        // G1 missing => explicit skip; G2 present => preserve the exact value
        // (the driver validates the URI); G3 invalid encoding => failure with
        // no value disclosure. Input injection avoids process-global env races.
        assert_eq!(
            admit_postgres_fixture(Err(std::env::VarError::NotPresent), false),
            None
        );
        assert_eq!(
            admit_postgres_fixture(Ok("exact-value".into()), false),
            Some("exact-value".into())
        );
        let failure = std::panic::catch_unwind(|| {
            admit_postgres_fixture(
                Err(std::env::VarError::NotUnicode(
                    "never disclose this value".into(),
                )),
                false,
            )
        })
        .unwrap_err();
        assert_eq!(
            failure.downcast_ref::<&str>().copied(),
            Some("AWAKEN_TEST_DATABASE_URL must be valid UTF-8")
        );
    }

    #[test]
    fn required_postgres_fixture_rejects_absence_without_disclosing_values() {
        // F1 required+absent => failure; F2 required+configured => exact value.
        // Inject the inputs instead of racing process-global environment writes.
        let error = std::panic::catch_unwind(|| {
            admit_postgres_fixture(Err(std::env::VarError::NotPresent), true)
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<&str>().copied(),
            Some("required PostgreSQL fixture AWAKEN_TEST_DATABASE_URL is not configured")
        );
        assert_eq!(
            admit_postgres_fixture(Ok("exact-value".into()), true),
            Some("exact-value".into())
        );
    }

    #[test]
    fn required_switch_distinguishes_absence_one_and_invalid() {
        assert!(!postgres_fixture_required(Err(
            std::env::VarError::NotPresent
        )));
        assert!(postgres_fixture_required(Ok("1".into())));
        for invalid in [
            Ok("".into()),
            Ok("0".into()),
            Ok("true".into()),
            Err(std::env::VarError::NotUnicode(
                "never disclose this value".into(),
            )),
        ] {
            let error =
                std::panic::catch_unwind(|| postgres_fixture_required(invalid)).unwrap_err();
            assert_eq!(
                error.downcast_ref::<&str>().copied(),
                Some("AWAKEN_TEST_REQUIRE_POSTGRES must be 1 or unset")
            );
        }
    }

    #[tokio::test]
    async fn postgres_fixture_pool_connects_when_configured() {
        let Some(url) = postgres_fixture_url() else {
            return;
        };
        let pool = postgres_fixture_pool(&url).await;
        let observed: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(observed, 1);
        pool.close().await;
    }

    #[test]
    fn postgres_fixture_pool_admission_respects_configuration() {
        assert_postgres_fixture_child_contract(
            "fixture::tests::postgres_fixture_pool_connects_when_configured",
        );
    }

    #[test]
    fn namespace_names_are_unique_unquoted_identifiers_appended_to_the_url() {
        // N1 two admissions => distinct names; N2 names need no quoting and fit
        // PostgreSQL's 63-byte identifier limit; N3 a URL without parameters
        // gains `?options=`; N4 configured parameters are kept, then `&options=`.
        let (first, second) = (fresh_namespace(), fresh_namespace());
        assert_ne!(first, second, "N1");
        for name in [&first, &second] {
            assert!(name.len() <= 63, "N2 length {name}");
            assert!(
                name.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'),
                "N2 charset {name}"
            );
        }
        assert_eq!(
            namespaced_url("postgres://h/db", "awaken_fixture_a"),
            "postgres://h/db?options=-c%20search_path%3Dawaken_fixture_a",
            "N3"
        );
        assert_eq!(
            namespaced_url("postgres://h/db?sslmode=disable", "awaken_fixture_a"),
            "postgres://h/db?sslmode=disable&options=-c%20search_path%3Dawaken_fixture_a",
            "N4"
        );
    }

    #[tokio::test]
    async fn admitted_fixtures_cannot_see_each_others_rows() {
        // I1 each admission selects its own non-public schema; I2 a table and
        // row written through one admission are invisible to the other, even
        // under the same unqualified name; I3 the other admission's own rows
        // remain exactly its own. This is the property recovery/archive scans
        // over "every row" rely on in a database shared by many tests.
        let Some(left_url) = postgres_fixture_url() else {
            return;
        };
        let right_url = postgres_fixture_url().expect("second admission of a configured fixture");
        let left = postgres_fixture_pool(&left_url).await;
        let right = postgres_fixture_pool(&right_url).await;
        let schema = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, String>("SELECT current_schema()")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let (left_schema, right_schema) = (schema(left.clone()).await, schema(right.clone()).await);
        assert_ne!(left_schema, right_schema, "I1");
        assert_ne!(left_schema, "public", "I1");
        for (pool, row) in [(&left, "left"), (&right, "right")] {
            sqlx::query("CREATE TABLE fixture_probe (row_id TEXT PRIMARY KEY)")
                .execute(pool)
                .await
                .expect("I2 same unqualified table name in each namespace");
            sqlx::query("INSERT INTO fixture_probe (row_id) VALUES ($1)")
                .bind(row)
                .execute(pool)
                .await
                .unwrap();
        }
        for (pool, row) in [(&left, "left"), (&right, "right")] {
            let rows: Vec<String> = sqlx::query_scalar("SELECT row_id FROM fixture_probe")
                .fetch_all(pool)
                .await
                .unwrap();
            assert_eq!(rows, [row], "I2/I3");
        }
        left.close().await;
        right.close().await;
    }

    #[test]
    fn admitted_fixture_isolation_respects_configuration() {
        assert_postgres_fixture_child_contract(
            "fixture::tests::admitted_fixtures_cannot_see_each_others_rows",
        );
    }
}
