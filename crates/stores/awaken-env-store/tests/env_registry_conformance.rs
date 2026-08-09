//! Trait-generic **conformance suite** for the `EnvRegistry` port — the shared behavioural
//! contract every backend must satisfy, run against both the in-memory reference and the
//! SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! It lifts the Phase-1 `proptest` invariants (which fuzz only the in-memory backend) to
//! the PORT: the durable sqlite registry must be observably identical to the reference on
//! the archive-vs-delete distinction and fail-closed lookups. A divergence — e.g. a
//! durable `delete` that soft-hides instead of removing, or an `archive` that drops a
//! record `get` should still return — is a real cross-deployment inconsistency, caught
//! here as a contract violation. Postgres joins behind its DB harness.

use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_session_contract::env_registry::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvRegistry,
    EnvUpdate, EnvironmentConfig, EnvironmentConfigMutation, EnvironmentNetworking,
    EnvironmentNetworkingMutation, EnvironmentPackages, EnvironmentPackagesMutation,
    EnvironmentRevision,
};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

async fn make<R: EnvRegistry>(r: &R, name: &str) -> String {
    r.create(
        name.into(),
        String::new(),
        Default::default(),
        EnvironmentConfig::SelfHosted,
    )
    .await
    .id
}

// ── The port contract, trait-generic over any EnvRegistry backend ────────────────

/// Distinct ids across creates, all retrievable (no id reuse regardless of name).
async fn unique_ids<R: EnvRegistry>(r: &R) {
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(make(r, "same-name").await);
    }
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 5, "ids collided");
    for id in &ids {
        assert!(r.exists(id).await, "a created id is missing");
    }
}

/// Archive is SOFT (record stays retrievable, leaves list_active); delete is HARD.
async fn archive_soft_delete_hard<R: EnvRegistry>(r: &R) {
    let a = make(r, "a").await;
    let d = make(r, "d").await;
    assert!(r.archive(&a).await.is_some());
    assert!(
        r.get(&a).await.is_some(),
        "archive must keep the record retrievable"
    );
    let active: Vec<String> = r.list_active().await.into_iter().map(|e| e.id).collect();
    assert!(
        !active.contains(&a),
        "archived record must leave list_active"
    );
    assert!(active.contains(&d), "a live record stays in list_active");
    assert!(r.delete(&d).await, "delete of an existing id reports true");
    assert!(r.get(&d).await.is_none(), "delete must remove the record");
}

/// Fail-closed: archive/update/delete on a never-created id → None/false.
async fn missing_id_fails_closed<R: EnvRegistry>(r: &R) {
    assert!(r.archive("env_missing").await.is_none());
    assert!(r.update("env_missing", Default::default()).await.is_none());
    assert!(!r.delete("env_missing").await);
    assert!(!r.exists("env_missing").await);
}

/// Delete is idempotent: the second delete reports false, nothing resurrects.
async fn delete_idempotent<R: EnvRegistry>(r: &R) {
    let id = make(r, "e").await;
    assert!(r.delete(&id).await, "first delete true");
    assert!(!r.delete(&id).await, "second delete false");
    assert!(r.get(&id).await.is_none());
}

async fn revision_decision_table<R: EnvRegistry>(r: &R) {
    // Cause-effect graph:
    // C1 create -> E1 revision 1; C2 authored update -> E2 increment once;
    // C3 first archive -> E3 increment once; C4 repeated archive -> E4 replay.
    // Missing targets produce no record and therefore no invented revision.
    //
    // | Rule | Trigger          | Exists | Already archived | Revision/result |
    // |------|------------------|--------|------------------|-----------------|
    // | V1   | create           | -      | -                | 1               |
    // | V2   | update           | T      | F                | 2               |
    // | V3   | archive          | T      | F                | 3               |
    // | V4   | archive replay   | T      | T                | 3               |
    // | V5   | update missing   | F      | -                | None            |
    let item = r
        .create(
            "versioned".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::SelfHosted,
        )
        .await;
    assert_eq!(item.revision, EnvironmentRevision(1), "V1");
    let item = r
        .update(
            &item.id,
            EnvUpdate {
                name: Some("versioned-2".into()),
                ..Default::default()
            },
        )
        .await
        .expect("V2 update");
    assert_eq!(item.revision, EnvironmentRevision(2), "V2");
    let item = r.archive(&item.id).await.expect("V3 archive");
    assert_eq!(item.revision, EnvironmentRevision(3), "V3");
    assert_eq!(
        r.archive(&item.id).await.unwrap().revision,
        EnvironmentRevision(3),
        "V4"
    );
    assert!(
        r.update("env_missing", EnvUpdate::default())
            .await
            .is_none(),
        "V5"
    );
}

async fn scope_round_trips_and_updates<R: EnvRegistry>(r: &R) {
    let item = r
        .create_scoped(
            "scoped".into(),
            String::new(),
            Default::default(),
            Some("organization".into()),
            EnvironmentConfig::SelfHosted,
        )
        .await;
    assert_eq!(
        r.get(&item.id).await.unwrap().scope.as_deref(),
        Some("organization")
    );
    let updated = r
        .update(
            &item.id,
            EnvUpdate {
                scope: Some(Some("account".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.scope.as_deref(), Some("account"));
    assert_eq!(updated.revision, EnvironmentRevision(2));
}

async fn nested_config_patch_is_atomic_and_durable<R: EnvRegistry>(r: &R) {
    // Cause graph / decision table (run unchanged against memory and SQLite):
    // | Rule | Present mutation | Omitted sibling | Durable reread |
    // | P1 | MCP=false | hosts/package flag | siblings preserved |
    // | P2 | npm=null | pip | npm cleared, pip preserved |
    let item = r
        .create(
            "patch".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::Cloud {
                networking: EnvironmentNetworking::Limited {
                    allowed_hosts: vec!["api.example.test".into()],
                    allow_mcp_servers: true,
                    allow_package_managers: true,
                },
                packages: EnvironmentPackages {
                    npm: vec!["tsx".into()],
                    pip: vec!["httpx".into()],
                    ..Default::default()
                },
            },
        )
        .await;
    r.update(
        &item.id,
        EnvUpdate {
            config: Some(EnvironmentConfigMutation::PatchCloud {
                networking: Some(EnvironmentNetworkingMutation::Limited {
                    allowed_hosts: None,
                    allow_mcp_servers: Some(Some(false)),
                    allow_package_managers: None,
                }),
                packages: Some(EnvironmentPackagesMutation {
                    npm: Some(None),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        },
    )
    .await
    .expect("atomic patch");
    let reread = r.get(&item.id).await.expect("durable reread");
    let EnvironmentConfig::Cloud {
        networking:
            EnvironmentNetworking::Limited {
                allowed_hosts,
                allow_mcp_servers,
                allow_package_managers,
            },
        packages,
    } = reread.config
    else {
        panic!("patch must retain cloud/limited variants")
    };
    assert_eq!(allowed_hosts, vec!["api.example.test"], "P1");
    assert!(!allow_mcp_servers, "P1");
    assert!(allow_package_managers, "P1");
    assert!(packages.npm.is_empty(), "P2");
    assert_eq!(packages.pip, vec!["httpx"], "P2");
}

async fn idempotent_create_decision_table<R: EnvRegistry>(r: &R) {
    // Cause/effect graph: a new command creates one row; the same id and exact
    // payload replays that row; the same id with another payload conflicts and
    // creates no row. This runs unchanged against memory and SQLite.
    let command = CreateEnvironmentCommand {
        command_id: "command-1".into(),
        name: "stable".into(),
        description: String::new(),
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
    };
    let CreateEnvironmentOutcome::Created(created) = r.create_once(command.clone()).await.unwrap()
    else {
        panic!("R1 must create")
    };
    let replayed = r.create_once(command.clone()).await.unwrap();
    assert_eq!(replayed.item().id, created.id, "R2 exact replay");
    let mut conflicting = command;
    conflicting.name = "different".into();
    assert_eq!(
        r.create_once(conflicting).await.unwrap_err(),
        CreateEnvironmentError::IdempotencyConflict,
        "R3 conflicting reuse"
    );
    assert_eq!(r.list_active().await.len(), 1, "R1-R3 one effect");
}

async fn run_suite<R: EnvRegistry>(fresh: impl Fn() -> R) {
    unique_ids(&fresh()).await;
    archive_soft_delete_hard(&fresh()).await;
    missing_id_fails_closed(&fresh()).await;
    delete_idempotent(&fresh()).await;
    revision_decision_table(&fresh()).await;
    scope_round_trips_and_updates(&fresh()).await;
    nested_config_patch_is_atomic_and_durable(&fresh()).await;
    idempotent_create_decision_table(&fresh()).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(InMemoryEnvRegistry::new));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(|| {
        SqliteEnvRegistry::open_in_memory().expect("sqlite in-memory registry")
    }));
}
