//! Browser-facing protocol admission resolved from deployment configuration.

use super::Role;

pub(super) fn resolve(
    origins: Option<Vec<String>>,
    role: Role,
) -> Result<(awaken_coordinator::AiSdkBrowserCors, &'static str), String> {
    let configured = origins.is_some();
    let cors = awaken_coordinator::AiSdkBrowserCors::try_from_origins(origins.unwrap_or_default())?;
    if !cors.origins().is_empty() && !matches!(role, Role::AllInOne | Role::Coordinator) {
        return Err(
            "ai_sdk_browser_origins is valid only for all-in-one or coordinator roles".to_owned(),
        );
    }
    Ok((
        cors,
        if configured {
            "config.toml"
        } else {
            "default closed"
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_are_explicit_and_owned_by_runtime_roles() {
        // Test design — C1 origins omitted/present, C2 owning/non-owning role, and C3 exact/unsafe
        // origin form produce: E1 default-closed empty policy; E2 exact ordered allowlist retained;
        // E3 non-runtime roles reject ignored authority; E4 the coordinator value object rejects
        // wildcard, path, or cleartext remote origins before listener startup.
        let (closed, source) = resolve(None, Role::AllInOne).unwrap();
        assert!(closed.origins().is_empty(), "!C1=>E1");
        assert_eq!(source, "default closed", "!C1=>E1");

        let configured = Some(vec![
            "https://one.workspace.example".to_owned(),
            "https://two.workspace.example:8443".to_owned(),
        ]);
        for role in [Role::AllInOne, Role::Coordinator] {
            let (open, source) = resolve(configured.clone(), role).unwrap();
            assert_eq!(
                open.origins(),
                [
                    "https://one.workspace.example",
                    "https://two.workspace.example:8443"
                ],
                "C1+C2+C3=>E2"
            );
            assert_eq!(source, "config.toml", "C1+C2+C3=>E2");
        }

        for role in [Role::Control, Role::Worker] {
            let error =
                resolve(Some(vec!["https://workspace.example".to_owned()]), role).unwrap_err();
            assert!(
                error.contains("only for all-in-one or coordinator"),
                "C1+!C2=>E3"
            );
        }

        for invalid in [
            "*",
            "http://workspace.example",
            "https://workspace.example/path",
        ] {
            let error = resolve(Some(vec![invalid.to_owned()]), Role::AllInOne).unwrap_err();
            assert!(
                error.contains("ai_sdk_browser_origins"),
                "C1+C2+!C3=>E4: {error}"
            );
        }
    }
}
