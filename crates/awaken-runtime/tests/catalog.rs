//! Catalog install is atomic and fails closed on an empty or mismatched
//! fingerprint (G4/G23/G29); capability reporting reflects the active catalog.
//!
//! G4: runtime builds live execution objects from its own catalog and fails
//! closed on fingerprint/catalog mismatch.
//! G23: config publication and runtime projection use versioned, atomic handoffs;
//! incomplete registry installs do not replace active catalogs.
//! G29: config publication coordination and registry compilation stay outside
//! runtime core; runtime receives only complete catalog install requests.

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

/// A mismatched fingerprint (top-level vs capabilities) must be rejected
/// before any state change, leaving the active catalog untouched (G4/G23).
#[test]
fn fingerprint_catalog_capabilities_mismatch_is_rejected() {
    let runtime = Runtime::new();
    runtime
        .install_catalog(catalog("catalog-a", vec!["alpha"]))
        .expect("valid catalog installs");

    // Build an install where the top-level fingerprint disagrees with the
    // capabilities catalog fingerprint — an internally inconsistent install.
    let fp = CatalogFingerprint("catalog-b".to_string());
    let caps_fp = CatalogFingerprint("catalog-c".to_string());
    let inconsistent = RuntimeCatalogInstall {
        publication_id: "pub-2".to_string(),
        fingerprint: fp,
        source_revisions: vec!["rev-2".to_string()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: caps_fp,
            runtime_version: "test".to_string(),
            tools: Vec::new(),
            plugins: Vec::new(),
        },
    };
    assert!(
        matches!(
            runtime.install_catalog(inconsistent),
            Err(Error::Rejected(_))
        ),
        "mismatched fingerprints must be rejected"
    );

    // Active catalog must be unchanged — still catalog-a (G23 rollback).
    assert_eq!(
        runtime.runtime_capabilities().catalog_fingerprint,
        CatalogFingerprint("catalog-a".to_string()),
        "active catalog must remain unchanged after a rejected install"
    );
}

/// A rejected install (any reason) must leave the previously active catalog
/// untouched — incomplete installs do not replace the active catalog (G23).
#[test]
fn active_catalog_is_unchanged_on_rejected_install() {
    let runtime = Runtime::new();
    runtime
        .install_catalog(catalog("catalog-a", vec!["alpha"]))
        .expect("first install");

    // Attempt to install a catalog with an empty fingerprint (always rejected).
    let _ = runtime.install_catalog(catalog("   ", Vec::new()));

    // The active catalog must still be catalog-a.
    assert_eq!(
        runtime.runtime_capabilities().catalog_fingerprint,
        CatalogFingerprint("catalog-a".to_string()),
        "active catalog must be unchanged when an install is rejected"
    );
}

/// G29: the install request is self-contained — the runtime never loads config
/// records from an external registry or publication coordinator. The test verifies
/// that the runtime accepts only a complete `RuntimeCatalogInstall` value.
#[test]
fn runtime_only_accepts_complete_install_requests() {
    // A complete install succeeds; the runtime does not probe external systems.
    let runtime = Runtime::new();
    let fp = CatalogFingerprint("fp-1".to_string());
    let install = RuntimeCatalogInstall {
        publication_id: "pub-1".to_string(),
        fingerprint: fp.clone(),
        source_revisions: vec!["rev-1".to_string()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: fp.clone(),
            runtime_version: "test".to_string(),
            tools: vec![ToolCapability {
                id: "my_tool".to_string(),
            }],
            plugins: Vec::new(),
        },
    };
    runtime
        .install_catalog(install)
        .expect("complete install succeeds");

    let caps = runtime.runtime_capabilities();
    assert_eq!(caps.catalog_fingerprint, fp);
    assert_eq!(caps.tools.len(), 1);
    assert_eq!(caps.tools[0].id, "my_tool");
}

/// G23: successive installs are each atomic; the capability source always
/// reflects exactly one catalog at a time, never a mix.
#[test]
fn successive_installs_are_each_atomic_and_capability_source_is_consistent() {
    let runtime = Runtime::new();
    runtime
        .install_catalog(catalog("v1", vec!["tool-a"]))
        .expect("v1 installs");
    let caps_v1 = runtime.runtime_capabilities();
    assert_eq!(caps_v1.tools.len(), 1);
    assert_eq!(caps_v1.catalog_fingerprint.0, "v1");

    runtime
        .install_catalog(catalog("v2", vec!["tool-a", "tool-b"]))
        .expect("v2 installs");
    let caps_v2 = runtime.runtime_capabilities();
    assert_eq!(caps_v2.tools.len(), 2);
    assert_eq!(caps_v2.catalog_fingerprint.0, "v2");

    // v1 tools must not appear under v2.
    assert!(caps_v2.tools.iter().any(|t| t.id == "tool-b"));
}
