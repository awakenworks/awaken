//! Behavior tests for the Docker package-image builder's deployment settings.

use super::*;

#[test]
fn registry_auth_selects_only_the_configured_registry_without_exposing_it() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        r#"{"auths":{"https://registry.internal/":{"auth":"dXNlcjpwYXNz"}}}"#,
    )
    .unwrap();
    let builder =
        DockerPackageImageProvisioner::with_client(Docker::connect_with_local_defaults().unwrap())
            .with_package_registry("registry.internal")
            .with_package_registry_auth_file(file.path())
            .unwrap();
    let credentials = builder.package_registry_credentials.unwrap();
    assert_eq!(credentials.auth.as_deref(), Some("dXNlcjpwYXNz"));
    assert_eq!(credentials.username.as_deref(), Some("user"));
    assert_eq!(credentials.password.as_deref(), Some("pass"));
    assert_eq!(
        credentials.serveraddress.as_deref(),
        Some("https://registry.internal/")
    );
}

#[test]
fn package_network_opt_in_preserves_environment_egress_boundary() {
    // Cause/effect table: host opt-in false/true crossed with Unrestricted,
    // None, Allowlist. Only Unrestricted+true selects host. None always
    // selects none; Allowlist always rejects. Sandbox execution networking is the
    // Docker Sandbox adapter's, not this builder's.
    for enabled in [false, true] {
        assert_eq!(
            package_build_network(&pc::NetworkPolicy::Unrestricted, enabled).unwrap(),
            if enabled { "host" } else { "default" }
        );
        assert_eq!(
            package_build_network(&pc::NetworkPolicy::None, enabled).unwrap(),
            "none"
        );
        assert!(
            package_build_network(
                &pc::NetworkPolicy::Allowlist {
                    hosts: vec!["pypi.org".into()]
                },
                enabled
            )
            .is_err()
        );
        let rt = DockerPackageImageProvisioner::with_client(
            Docker::connect_with_local_defaults().unwrap(),
        );
        assert!(!rt.package_build_host_network);
        assert_eq!(
            rt.with_package_build_host_network(enabled)
                .package_build_host_network,
            enabled
        );
    }
}

#[test]
fn package_build_deadline_is_deployment_selected() {
    // D1 the default bounds a build at the shared engine deadline; D2 the
    // deployment's explicit deadline replaces it.
    let docker = Docker::connect_with_local_defaults().unwrap();
    let builder = DockerPackageImageProvisioner::with_client(docker);
    assert_eq!(builder.package_build_timeout, PACKAGE_BUILD_TIMEOUT, "D1");
    let builder = builder.with_package_build_timeout(std::time::Duration::from_secs(17));
    assert_eq!(
        builder.package_build_timeout,
        std::time::Duration::from_secs(17),
        "D2"
    );
}
