//! Admission of an explicitly configured verification fixture. The existing
//! database runner provisions it; each domain still owns its schema and laws.

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
