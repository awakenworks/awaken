//! Explicit admission for real NATS tests; domain suites own the protocol laws.

use std::env::VarError;
use std::io::Write as _;
use std::process::Command;

const SKIPPED: &str = "SKIPPED NATS fixture: AWAKEN_TEST_NATS_URL is not configured";
const EXERCISED: &str = "VERIFIED live NATS fixture";

/// An absent optional fixture is a reported skip. Explicit configuration and a
/// required fixture must never turn a connection or protocol failure into a skip.
#[expect(
    clippy::disallowed_methods,
    reason = "the reliability fixture reads which live service the test run provides from the environment"
)]
pub fn nats_fixture_url() -> Option<String> {
    let url = admit(
        std::env::var("AWAKEN_TEST_NATS_URL"),
        std::env::var("AWAKEN_TEST_REQUIRE_NATS"),
    );
    if url.is_none() {
        writeln!(std::io::stderr(), "{SKIPPED}").expect("report NATS fixture absence");
    }
    url
}

/// Emit only after the domain's real message/tool assertions have succeeded.
pub fn report_nats_fixture_exercised() {
    eprintln!("{EXERCISED}");
}

fn admit(url: Result<String, VarError>, required: Result<String, VarError>) -> Option<String> {
    let required = match required {
        Err(VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        _ => panic!("AWAKEN_TEST_REQUIRE_NATS must be 1 or unset"),
    };
    match url {
        Ok(value) if !value.trim().is_empty() => Some(value),
        Ok(_) => panic!("configured NATS fixture URL must not be empty"),
        Err(VarError::NotPresent) if !required => None,
        Err(VarError::NotPresent) => {
            panic!("required NATS fixture AWAKEN_TEST_NATS_URL is not configured")
        }
        Err(VarError::NotUnicode(_)) => panic!("AWAKEN_TEST_NATS_URL must be valid UTF-8"),
    }
}

/// Cross the actual test-process boundary for every admission outcome. Invalid
/// URLs are intentional loopback fixtures; never disclose a caller's live URL.
#[expect(
    clippy::disallowed_methods,
    reason = "the reliability fixture reads which live service the test run provides from the environment"
)]
pub fn assert_nats_fixture_child_contract(test_name: &str) {
    let run = |url: Option<&str>, required: bool| {
        let mut child = Command::new(std::env::current_exe().expect("NATS test executable"));
        child
            .args(["--exact", test_name, "--nocapture"])
            .env_remove("AWAKEN_TEST_NATS_URL")
            .env_remove("AWAKEN_TEST_REQUIRE_NATS");
        if let Some(url) = url {
            child.env("AWAKEN_TEST_NATS_URL", url);
        }
        if required {
            child.env("AWAKEN_TEST_REQUIRE_NATS", "1");
        }
        child.output().expect("NATS admission child")
    };
    // Cause/effect N1 optional absence -> success with explicit skip; N2 required
    // absence -> failure; N3 present empty/unreachable -> failure even when
    // optional; N4 required live -> real successful domain assertion and marker.
    let optional = run(None, false);
    assert!(optional.status.success(), "N1 optional absence");
    assert!(String::from_utf8_lossy(&optional.stderr).contains(SKIPPED));
    let missing = run(None, true);
    assert!(!missing.status.success(), "N2 required absence");
    assert!(String::from_utf8_lossy(&missing.stderr).contains("required NATS fixture"));
    for required in [false, true] {
        for url in ["", "nats://127.0.0.1:1"] {
            let failed = run(Some(url), required);
            assert!(!failed.status.success(), "N3 configured fixture must fail");
            assert!(!String::from_utf8_lossy(&failed.stderr).contains(SKIPPED));
        }
    }
    if let Ok(url) = std::env::var("AWAKEN_TEST_NATS_URL") {
        let live = run(Some(&url), true);
        assert!(live.status.success(), "N4 configured live fixture");
        assert!(String::from_utf8_lossy(&live.stderr).contains(EXERCISED));
        assert!(!String::from_utf8_lossy(&live.stderr).contains(SKIPPED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_admission_decision_table_preserves_exact_urls_and_rejects_missing_premises() {
        assert_eq!(
            admit(Err(VarError::NotPresent), Err(VarError::NotPresent)),
            None
        );
        for required in [Err(VarError::NotPresent), Ok("1".into())] {
            assert_eq!(
                admit(Ok("nats://exact-value".into()), required),
                Some("nats://exact-value".into())
            );
        }
        for (url, required) in [
            (Err(VarError::NotPresent), Ok("1".into())),
            (Ok(String::new()), Err(VarError::NotPresent)),
            (Ok("  ".into()), Ok("1".into())),
            (Ok("never disclose this value".into()), Ok("0".into())),
            (
                Err(VarError::NotUnicode("never disclose this value".into())),
                Ok("1".into()),
            ),
        ] {
            let failure = std::panic::catch_unwind(|| admit(url, required)).unwrap_err();
            let message = failure.downcast_ref::<&str>().copied().unwrap_or("");
            assert!(!message.contains("never disclose"));
        }
    }
}
