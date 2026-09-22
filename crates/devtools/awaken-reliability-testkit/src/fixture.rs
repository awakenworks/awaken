//! Admission of an explicitly configured verification fixture. The existing
//! database runner provisions it; each domain still owns its schema and laws.

/// No configuration is an explicit local skip. Invalid configuration is not
/// absence. Once Some is returned, callers must require connection/setup to
/// succeed rather than erase either failure into a skipped test.
pub fn postgres_fixture_url() -> Option<String> {
    let configured = admit_postgres_fixture(std::env::var("AWAKEN_TEST_DATABASE_URL"));
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

fn admit_postgres_fixture(value: Result<String, std::env::VarError>) -> Option<String> {
    match value {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
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
            admit_postgres_fixture(Err(std::env::VarError::NotPresent)),
            None
        );
        assert_eq!(
            admit_postgres_fixture(Ok("exact-value".into())),
            Some("exact-value".into())
        );
        let failure = std::panic::catch_unwind(|| {
            admit_postgres_fixture(Err(std::env::VarError::NotUnicode(
                "never disclose this value".into(),
            )))
        })
        .unwrap_err();
        assert_eq!(
            failure.downcast_ref::<&str>().copied(),
            Some("AWAKEN_TEST_DATABASE_URL must be valid UTF-8")
        );
    }
}
