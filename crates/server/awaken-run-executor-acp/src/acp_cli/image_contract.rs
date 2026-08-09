//! Production Sandbox image projection derived from the ACP CLI catalog.

#[cfg(test)]
use super::AcpAcquisition;
use super::known_acp_clis;

/// One exact package baked into the production Sandbox image for an ACP row.
/// Launch/probe argv and executable discovery remain derived from `AcpCli`; this
/// value carries only the image-specific package-manager fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AcpImageRequirement {
    pub manager: &'static str,
    pub requirement: &'static str,
}

/// Generate the immutable production-image contract from the authoritative ACP
/// catalog. Build tooling writes this value into the image; no checked-in JSON
/// mirror exists.
pub fn image_runtime_contract_json() -> Result<String, serde_json::Error> {
    let runtimes = known_acp_clis()
        .iter()
        .map(|cli| {
            let mut executables = std::collections::BTreeSet::new();
            if let Some(executable) = cli.container_argv.first() {
                executables.insert(*executable);
            }
            executables.insert(cli.discovery.version.executable);
            serde_json::json!({
                "id": cli.id,
                "requirements": cli.image_requirements,
                "executables": executables,
                "probe_argv": cli.container_probe_argv.unwrap_or(cli.container_argv),
                "auth_method_id": cli.capability_probe_auth_method_id,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({
        "schema_version": 1,
        "runtimes": runtimes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_image_contract_is_derived_from_every_catalog_row() {
        // Image-contract FMECA/cause-effect table: C1 a catalog row has exact
        // requirements; C2 launch/probe/auth facts exist on that same row; C3 a
        // wrapper acquisition package is required. Effects: E1 one generated
        // image row; E2 exact probe/auth/executables; E3 wrapper package included.
        // Rules I1 C1+C2=>E1+E2; I2 C3=>E3. There is no deserialized mirror to
        // synchronize: this test exercises the actual build generator.
        let contract: serde_json::Value =
            serde_json::from_str(&image_runtime_contract_json().expect("generate contract"))
                .expect("generated JSON parses");
        let runtimes = contract["runtimes"].as_array().expect("runtime rows");
        assert_eq!(runtimes.len(), known_acp_clis().len(), "I1");
        for cli in known_acp_clis() {
            let image = runtimes
                .iter()
                .find(|runtime| runtime["id"] == cli.id)
                .unwrap_or_else(|| panic!("I1: missing image runtime {}", cli.id));
            assert_eq!(
                image["probe_argv"],
                serde_json::json!(cli.container_probe_argv.unwrap_or(cli.container_argv)),
                "I1/E2 {}",
                cli.id
            );
            assert_eq!(
                image["auth_method_id"],
                serde_json::json!(cli.capability_probe_auth_method_id),
                "I1/E2 {}",
                cli.id
            );
            assert!(!image["executables"].as_array().unwrap().is_empty(), "I1");
            assert!(!cli.image_requirements.is_empty(), "I1: {}", cli.id);
            for requirement in cli.image_requirements {
                assert!(matches!(requirement.manager, "npm" | "pip"), "I1");
                assert!(
                    !requirement.requirement.contains("@latest")
                        && !requirement.requirement.ends_with("=="),
                    "I1: {} image requirement must be exact",
                    cli.id
                );
            }
            if let AcpAcquisition::PinnedNpmWrapper { package, .. } = cli.acquisition {
                assert!(
                    cli.image_requirements
                        .iter()
                        .any(|requirement| requirement.manager == "npm"
                            && requirement.requirement == package),
                    "I2: {}",
                    cli.id
                );
            }
        }
    }
}
