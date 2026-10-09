//! Admission of an explicitly configured verification fixture. The existing
//! database runner provisions it; each domain still owns its schema and laws.

use std::process::{Command, Output};

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
pub fn postgres_fixture_url() -> Option<String> {
    let required = match std::env::var("AWAKEN_TEST_REQUIRE_POSTGRES") {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        _ => panic!("AWAKEN_TEST_REQUIRE_POSTGRES must be 1 or unset"),
    };
    let configured = admit_postgres_fixture(std::env::var("AWAKEN_TEST_DATABASE_URL"), required);
    if configured.is_none() {
        // Direct stderr stays visible even when libtest captures successful
        // tests' print macros. A local skip must not look like backend evidence.
        use std::io::Write as _;
        writeln!(
            std::io::stderr(),
            "SKIPPED PostgreSQL fixture: AWAKEN_TEST_DATABASE_URL is not configured"
        )
        .expect("report PostgreSQL fixture admission");
    }
    configured
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
}
