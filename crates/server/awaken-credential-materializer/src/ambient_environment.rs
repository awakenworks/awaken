//! Process-boundary guard against provider SDK environment fallback.

use std::ffi::OsString;

/// Ambient provider API-key variables are not an Awaken credential source.
/// Product processes invoke this before constructing any provider SDK or
/// Worker so future adapters cannot accidentally revive SDK environment
/// fallback. Only variable names are inspected; values are never read.
pub fn reject_ambient_api_key_environment() -> Result<(), AmbientApiKeyEnvironmentError> {
    reject_ambient_api_key_names(std::env::vars_os().map(|(name, _)| name))
}

fn reject_ambient_api_key_names(
    names: impl IntoIterator<Item = OsString>,
) -> Result<(), AmbientApiKeyEnvironmentError> {
    let mut forbidden = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| {
            let normalized = name.to_ascii_uppercase();
            normalized == "API_KEY" || normalized.ends_with("_API_KEY")
        })
        .collect::<Vec<_>>();
    forbidden.sort_unstable();
    forbidden.dedup();
    if forbidden.is_empty() {
        Ok(())
    } else {
        Err(AmbientApiKeyEnvironmentError { forbidden })
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "ambient provider API-key variables are forbidden ({forbidden:?}); unset them and import credentials through Awaken provider-connections"
)]
pub struct AmbientApiKeyEnvironmentError {
    forbidden: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ambient-key cause/effect decision table and FMECA. Causes: C1 no API-key
    /// name; C2 exact `API_KEY`; C3 vendor/lowercase `*_API_KEY`; C4 similarly
    /// named file/fd or non-key token variables. Effects: E1 allow startup;
    /// E2 reject using names only, never values. Rules A1=C1|C4=>E1 and
    /// A2=C2|C3=>E2. FMECA: an inherited provider key is critical because an SDK
    /// default could bypass Vault selection; the product startup guard detects
    /// every provider-independent suffix while C4 prevents false credential-file
    /// and internal-token failures.
    #[test]
    fn ambient_api_key_names_fail_closed_without_matching_files_or_tokens() {
        assert!(
            reject_ambient_api_key_names([
                OsString::from("PATH"),
                OsString::from("DEEPSEEK_API_KEY_FILE"),
                OsString::from("PILOT_E2E_DEEPSEEK_API_KEY_FD"),
                OsString::from("AWAKEN_MCP_BEARER_TOKEN"),
            ])
            .is_ok(),
            "A1/E1"
        );
        let error = reject_ambient_api_key_names([
            OsString::from("API_KEY"),
            OsString::from("DEEPSEEK_API_KEY"),
            OsString::from("custom_api_key"),
        ])
        .expect_err("A2/E2");
        assert_eq!(
            error.forbidden,
            ["API_KEY", "DEEPSEEK_API_KEY", "custom_api_key"],
            "A2/E2 names only"
        );
    }
}
