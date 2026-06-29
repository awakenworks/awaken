//! Catalog install is atomic and fails closed on an empty fingerprint (G4/G23);
//! capability reporting reflects the active catalog.

use awaken_runtime::Runtime;
use awaken_runtime_contract::capability::{
    RuntimeCapabilityCatalog, RuntimeCapabilitySource, ToolCapability,
};
use awaken_runtime_contract::catalog::{Error, RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::resolved::CatalogFingerprint;

fn catalog(fingerprint: &str, tools: Vec<&str>) -> RuntimeCatalogInstall {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    RuntimeCatalogInstall {
        publication_id: "pub-1".to_string(),
        fingerprint: fingerprint.clone(),
        source_revisions: vec!["rev-1".to_string()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: fingerprint,
            runtime_version: "test".to_string(),
            tools: tools
                .into_iter()
                .map(|id| ToolCapability { id: id.to_string() })
                .collect(),
            plugins: Vec::new(),
        },
    }
}

#[test]
fn empty_fingerprint_is_rejected() {
    let runtime = Runtime::new();
    assert!(matches!(
        runtime.install_catalog(catalog("   ", Vec::new())),
        Err(Error::Rejected(_))
    ));
}

#[test]
fn capabilities_default_to_empty_without_a_catalog() {
    let runtime = Runtime::new();
    let caps = runtime.runtime_capabilities();
    assert!(caps.tools.is_empty());
    assert_eq!(caps.catalog_fingerprint, CatalogFingerprint(String::new()));
    // The reported runtime version is the crate version, not empty.
    assert!(!caps.runtime_version.is_empty());
}

#[test]
fn capabilities_reflect_the_active_catalog_and_install_is_atomic() {
    let runtime = Runtime::new();
    runtime
        .install_catalog(catalog("catalog-a", vec!["alpha"]))
        .expect("first install");
    assert_eq!(runtime.runtime_capabilities().tools.len(), 1);

    // A later install atomically replaces the active catalog.
    runtime
        .install_catalog(catalog("catalog-b", vec!["alpha", "beta"]))
        .expect("second install");
    let caps = runtime.runtime_capabilities();
    assert_eq!(caps.tools.len(), 2);
    assert_eq!(
        caps.catalog_fingerprint,
        CatalogFingerprint("catalog-b".to_string())
    );
}
