//! Test-only S3 admission. The caller owns the endpoint, bucket policy and laws.
use std::env::VarError;
use std::io::Write as _;

const REQUIRED: &str = "AWAKEN_TEST_REQUIRE_S3";
const ENDPOINT: &str = "AWAKEN_TEST_S3_ENDPOINT";
const KEY: &str = "AWAKEN_TEST_S3_ACCESS_KEY";
const SECRET: &str = "AWAKEN_TEST_S3_SECRET_KEY";
const OLD_SECRET: &str = "AWAKEN_TEST_S3_OLD_SECRET_KEY";
const ROTATION: &str = "AWAKEN_TEST_S3_ROTATION_ID";
const SKIPPED: &str = "SKIPPED S3 fixture: AWAKEN_TEST_S3_ENDPOINT is not configured";
const EXERCISED: &str = "VERIFIED live S3 fixture";

fn required(value: Result<String, VarError>) -> Result<bool, String> {
    match value {
        Err(VarError::NotPresent) => Ok(false),
        Ok(value) if value == "1" => Ok(true),
        _ => Err(format!("{REQUIRED} must be 1 or unset")),
    }
}

fn admit(
    name: &str,
    value: Result<String, VarError>,
    required: bool,
) -> Result<Option<String>, String> {
    match value {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Err(format!("configured {name} must not be empty")),
        Err(VarError::NotPresent) if !required => Ok(None),
        Err(VarError::NotPresent) => Err(format!("required {name} is not configured")),
        Err(VarError::NotUnicode(_)) => Err(format!("{name} must be valid UTF-8")),
    }
}

pub fn endpoint() -> Result<Option<String>, String> {
    let required = required(std::env::var(REQUIRED))?;
    let partial = [KEY, SECRET, OLD_SECRET, ROTATION]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    let result = admit(ENDPOINT, std::env::var(ENDPOINT), required || partial)?;
    if result.is_none() {
        writeln!(std::io::stderr(), "{SKIPPED}").expect("report optional S3 fixture absence");
    }
    Ok(result)
}

pub fn required_field(name: &str) -> Result<String, String> {
    admit(name, std::env::var(name), true)?
        .ok_or_else(|| format!("required {name} is not configured"))
}

pub fn current_secret() -> Result<Option<String>, String> {
    if endpoint()?.is_none() {
        return Ok(None);
    }
    required_field(SECRET).map(Some)
}

pub fn rotation_id() -> Result<Option<String>, String> {
    let value = admit(
        ROTATION,
        std::env::var(ROTATION),
        required(std::env::var(REQUIRED))?,
    )?;
    if value.is_none() {
        writeln!(
            std::io::stderr(),
            "SKIPPED S3 credential rotation: AWAKEN_TEST_S3_ROTATION_ID is not configured"
        )
        .expect("report optional rotation absence");
    }
    Ok(value)
}

pub fn report_exercised() {
    eprintln!("{EXERCISED}");
}

pub fn assert_child_contract(test_name: &str) {
    let run = |endpoint: Option<&str>, require: bool, live: bool| {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("S3 test executable"));
        command.args(["--exact", test_name, "--nocapture"]);
        for name in [REQUIRED, ENDPOINT, KEY, SECRET, OLD_SECRET, ROTATION] {
            command.env_remove(name);
        }
        if require {
            command.env(REQUIRED, "1");
        }
        if let Some(endpoint) = endpoint {
            command.env(ENDPOINT, endpoint);
            for name in [KEY, SECRET] {
                let value = if live {
                    required_field(name).expect("live S3 field")
                } else {
                    "fixture-value".to_owned()
                };
                command.env(name, value);
            }
        }
        command.output().expect("S3 fixture child")
    };
    let optional = run(None, false, false);
    assert!(optional.status.success(), "optional fixture absence");
    assert!(String::from_utf8_lossy(&optional.stderr).contains(SKIPPED));
    let missing = run(None, true, false);
    assert!(!missing.status.success(), "required fixture absence");
    assert!(!String::from_utf8_lossy(&missing.stderr).contains(SKIPPED));
    for require in [false, true] {
        for endpoint in ["", "http://127.0.0.1:1"] {
            let failed = run(Some(endpoint), require, false);
            assert!(!failed.status.success(), "configured fixture must fail");
            assert!(!String::from_utf8_lossy(&failed.stderr).contains(SKIPPED));
        }
    }
    if let Ok(endpoint) = std::env::var(ENDPOINT) {
        let live = run(Some(&endpoint), true, true);
        assert!(live.status.success(), "configured live S3 fixture");
        assert!(String::from_utf8_lossy(&live.stderr).contains(EXERCISED));
        assert!(!String::from_utf8_lossy(&live.stderr).contains(SKIPPED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn required_switch_has_one_canonical_value() {
        assert_eq!(required(Err(VarError::NotPresent)), Ok(false));
        assert_eq!(required(Ok("1".into())), Ok(true));
        for invalid in ["", "0", "true"] {
            assert!(required(Ok(invalid.into())).is_err());
        }
    }
    #[test]
    fn admission_table_preserves_values_and_rejects_missing_invalid_or_empty_fields() {
        assert_eq!(
            admit("field", Err(VarError::NotPresent), false).unwrap(),
            None
        );
        assert_eq!(
            admit("field", Ok(" exact value ".into()), true).unwrap(),
            Some(" exact value ".into())
        );
        for value in [
            Err(VarError::NotPresent),
            Ok(String::new()),
            Ok("  ".into()),
            Err(VarError::NotUnicode("never-disclose".into())),
        ] {
            let error = admit("field", value, true).unwrap_err();
            assert!(!error.contains("never-disclose"));
        }
        assert!(admit("field", Ok(String::new()), false).is_err());
    }
}
