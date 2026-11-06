//! T39-C01 oracle 1: every executable input, including egress networking,
//! participates in the exact recipe identity.
use super::*;
use awaken_environment_contract::{EnvItem, EnvironmentNetworking, EnvironmentPackages};

fn registration(networking: EnvironmentNetworking) -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(
        EnvItem {
            id: "env-a".into(),
            revision: EnvironmentRevision(1),
            name: "env".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::Cloud {
                networking,
                packages: EnvironmentPackages {
                    npm: vec!["tsx@4.0.0".into()],
                    ..Default::default()
                },
            },
            sandbox_policy: None,
            archived_at: None,
            created_at: Some("2026-02-03T04:05:06.007Z".into()),
            updated_at: Some("2026-02-03T04:05:06.007Z".into()),
        },
        None,
    )
}

fn limited(hosts: &[&str]) -> EnvironmentNetworking {
    EnvironmentNetworking::Limited {
        allowed_hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
        allow_mcp_servers: false,
        allow_package_managers: true,
    }
}

/// Negative-control mutant: a recipe key that forgets the networking input.
fn key_without_networking(demand: &EnvironmentImageBuildDemand) -> String {
    let packages = demand.config.packages();
    format!(
        "environment-image:{}",
        awaken_environment_contract::environment_facts_fingerprint(&(
            demand.base_image.as_str(),
            &packages,
        ))
    )
}

/// N1 unrestricted vs limited egress => distinct recipes; N2 two different
/// allowlists => distinct recipes; N3 the same allowlist => the same recipe.
/// The networking-blind mutant key collides on N1 and N2, which the
/// distinct-key oracle detects.
#[test]
fn networking_changes_select_a_distinct_image_recipe() {
    let base = "base@sha256:a";
    let open = EnvironmentImageBuildDemand::from_registration(
        &registration(EnvironmentNetworking::default()),
        base,
    )
    .unwrap();
    let docs = EnvironmentImageBuildDemand::from_registration(
        &registration(limited(&["docs.example"])),
        base,
    )
    .unwrap();
    let api = EnvironmentImageBuildDemand::from_registration(
        &registration(limited(&["api.example"])),
        base,
    )
    .unwrap();
    let docs_again = EnvironmentImageBuildDemand::from_registration(
        &registration(limited(&["docs.example"])),
        base,
    )
    .unwrap();

    assert!(!open.same_recipe(&docs), "N1");
    assert_ne!(open.build_key, docs.build_key, "N1");
    assert!(!docs.same_recipe(&api), "N2");
    assert_ne!(docs.build_key, api.build_key, "N2");
    assert!(docs.same_recipe(&docs_again), "N3");
    assert_eq!(docs.build_key, docs_again.build_key, "N3");

    assert_eq!(
        key_without_networking(&open),
        key_without_networking(&docs),
        "NC the networking-blind key aliases N1, which the N1 oracle rejects"
    );
    assert_eq!(
        key_without_networking(&docs),
        key_without_networking(&api),
        "NC N2"
    );
}
