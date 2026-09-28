// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-env-store/tests/env_registry_conformance.rs
// @formal reason: Trait-generic EnvRegistry conformance suite (ADR-0081 §7). It is a tooling-only test
// @formal reason: library every backend's test target calls; no product crate links or invokes it, and it
// @formal reason: admits nothing at runtime.
//! Trait-generic conformance suite for the `EnvRegistry` port (ADR-0081 §7).
//!
//! Every backend runs [`run_suite`] with its own `fresh_with` factory. Unique
//! ids, archive-preserves-history, fail-closed reads, the revision decision
//! table, registration-intent delivery and injected-clock observation are the
//! port's contract, so a divergence is a parity bug in the backend that shows it.

use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvRegistry,
    EnvUpdate, EnvironmentConfig, EnvironmentConfigMutation, EnvironmentFieldUpdate,
    EnvironmentNetworking, EnvironmentNetworkingMutation, EnvironmentPackages,
    EnvironmentPackagesMutation, EnvironmentRegistrationIntentFilter, EnvironmentRevision,
    EnvironmentSandboxPolicyRef, EnvironmentStoreError, InvalidEnvironmentConfig,
};
use awaken_lease_contract::WallClock;
use awaken_lease_contract::system_clock;
use awaken_lease_contract::utc::epoch_millis_to_rfc3339;
use awaken_lease_testkit::ManualClock;
use std::sync::Arc;

pub async fn make<R: EnvRegistry>(r: &R, name: &str) -> String {
    r.create(
        name.into(),
        String::new(),
        Default::default(),
        EnvironmentConfig::SelfHosted,
    )
    .await
    .expect("create Environment")
    .id
}

// ── The port contract, trait-generic over any EnvRegistry backend ────────────────

/// Distinct ids across creates, all retrievable (no id reuse regardless of name).
pub async fn unique_ids<R: EnvRegistry>(r: &R) {
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(make(r, "same-name").await);
    }
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 5, "ids collided");
    for id in &ids {
        assert!(
            r.exists(id).await.expect("read Environment"),
            "a created id is missing"
        );
    }
}

/// Terminal denial is an archive: history and exact lookup remain available.
pub async fn archive_preserves_authoritative_history<R: EnvRegistry>(r: &R) {
    let a = make(r, "a").await;
    assert!(r.archive(&a).await.expect("archive Environment").is_some());
    assert!(
        r.get(&a).await.expect("read Environment").is_some(),
        "archive must keep the record retrievable"
    );
    let active: Vec<String> = r
        .list_active()
        .await
        .expect("list Environments")
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert!(
        !active.contains(&a),
        "archived record must leave list_active"
    );
    assert!(
        r.get_revision(&a, awaken_environment_contract::EnvironmentRevision(1))
            .await
            .expect("read Environment revision")
            .is_some(),
        "the authored revision remains exact-readable"
    );
}

/// Fail-closed: archive/update on a never-created id → None.
pub async fn missing_id_fails_closed<R: EnvRegistry>(r: &R) {
    assert!(
        r.archive("env_missing")
            .await
            .expect("archive missing Environment")
            .is_none()
    );
    assert!(
        r.update(
            "env_missing",
            EnvironmentRevision::default(),
            Default::default(),
        )
        .await
        .expect("update missing Environment")
        .is_none()
    );
    assert!(
        !r.exists("env_missing")
            .await
            .expect("find missing Environment")
    );
}

pub async fn revision_decision_table<R: EnvRegistry>(r: &R) {
    // Cause-effect graph:
    // C1 create -> E1 revision 1; C2 authored update -> E2 increment once;
    // C3 stale expected revision -> E3 conflict and no write; C4 first archive
    // -> E4 increment once; C5 repeated archive -> E5 replay; C6 update after
    // archive -> E6 terminal denial with no new revision.
    // Missing targets produce no record and therefore no invented revision.
    //
    // | Rule | Trigger          | Exists | Already archived | Revision/result |
    // |------|------------------|--------|------------------|-----------------|
    // | V1   | create           | -      | -                | 1               |
    // | V2   | update           | T      | F                | 2               |
    // | V3   | update, stale r1 | T      | F                | conflict; stays 2|
    // | V4   | archive          | T      | F                | 3               |
    // | V5   | archive replay   | T      | T                | 3               |
    // | V6   | update archived  | T      | T                | None; stays at 3|
    // | V7   | update missing   | F      | -                | None            |
    let item = r
        .create(
            "versioned".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .expect("V1 create");
    assert_eq!(item.revision, EnvironmentRevision(1), "V1");
    assert_eq!(
        r.get_revision(&item.id, EnvironmentRevision(1))
            .await
            .expect("V1 read"),
        Some(item.clone()),
        "V1 immutable history"
    );
    let item = r
        .update(
            &item.id,
            item.revision,
            EnvUpdate {
                name: Some("versioned-2".into()),
                ..Default::default()
            },
        )
        .await
        .expect("V2 update store operation")
        .expect("V2 update");
    assert_eq!(item.revision, EnvironmentRevision(2), "V2");
    assert_eq!(
        r.update(
            &item.id,
            EnvironmentRevision(1),
            EnvUpdate {
                name: Some("stale-write".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err(),
        EnvironmentStoreError::RevisionConflict,
        "V3"
    );
    assert_eq!(
        r.get(&item.id).await.unwrap().unwrap(),
        item,
        "V3 leaves current state unchanged"
    );
    assert_eq!(
        r.get_revision(&item.id, EnvironmentRevision(1))
            .await
            .expect("V2 history read")
            .unwrap()
            .name,
        "versioned",
        "V2 does not rewrite V1"
    );
    let item = r
        .archive(&item.id)
        .await
        .expect("V3 archive store operation")
        .expect("V3 archive");
    assert_eq!(item.revision, EnvironmentRevision(3), "V3");
    assert!(
        r.get_revision(&item.id, EnvironmentRevision(2))
            .await
            .expect("V3 history read")
            .unwrap()
            .archived_at
            .is_none(),
        "V3 archive does not rewrite V2"
    );
    assert_eq!(
        r.archive(&item.id)
            .await
            .expect("V4 archive store operation")
            .unwrap()
            .revision,
        EnvironmentRevision(3),
        "V4"
    );
    assert!(
        r.update(
            &item.id,
            item.revision,
            EnvUpdate {
                name: Some("must-not-revive".into()),
                ..Default::default()
            }
        )
        .await
        .expect("V5 update store operation")
        .is_none(),
        "V5"
    );
    let terminal = r
        .get(&item.id)
        .await
        .expect("V5 terminal read")
        .expect("V5 terminal row retained");
    assert_eq!(terminal.revision, EnvironmentRevision(3), "V5");
    assert_eq!(terminal.name, item.name, "V5");
    assert!(
        r.update(
            "env_missing",
            EnvironmentRevision::default(),
            EnvUpdate::default(),
        )
        .await
        .expect("V6 update store operation")
        .is_none(),
        "V6"
    );
}

pub async fn sandbox_binding_is_one_environment_revision<R: EnvRegistry>(r: &R) {
    // Cause/effect decision table:
    // | Rule | Environment | exact policy ref | effect |
    // | B0 | create rev1 | p@1 | current and immutable rev1 both contain p@1 |
    // | B1 | existing rev1 | p@3 | atomically append rev2 containing p@3 |
    // | B2 | after B1 | read rev1 | no policy binding (history unchanged) |
    // | B3 | missing | p@3 | no row or invented revision |
    let initial_reference = EnvironmentSandboxPolicyRef {
        policy_id: "initial".into(),
        version: 1,
    };
    let created = r
        .create_once(CreateEnvironmentCommand {
            command_id: "policy-bound-at-create".into(),
            name: "initial binding".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
            sandbox_policy: Some(initial_reference.clone()),
        })
        .await
        .expect("B0 create")
        .into_item();
    assert_eq!(
        created.sandbox_policy,
        Some(initial_reference.clone()),
        "B0"
    );
    assert_eq!(
        r.get_revision(&created.id, EnvironmentRevision(1))
            .await
            .expect("B0 revision read")
            .unwrap()
            .sandbox_policy,
        Some(initial_reference),
        "B0"
    );

    let item = r
        .create(
            "policy-bound".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .expect("B1 create");
    let reference = EnvironmentSandboxPolicyRef {
        policy_id: "p".into(),
        version: 3,
    };
    let bound = r
        .update(
            &item.id,
            item.revision,
            EnvUpdate {
                sandbox_policy: Some(EnvironmentFieldUpdate::Replace(reference.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("B1 update store operation")
        .expect("B1");
    assert_eq!(bound.revision, EnvironmentRevision(2), "B1");
    assert_eq!(bound.sandbox_policy, Some(reference), "B1");
    assert_eq!(
        r.get_revision(&item.id, EnvironmentRevision(1))
            .await
            .expect("B2 history read")
            .unwrap()
            .sandbox_policy,
        None,
        "B2"
    );
    assert!(
        r.update(
            "env_missing",
            EnvironmentRevision::default(),
            EnvUpdate {
                sandbox_policy: Some(EnvironmentFieldUpdate::Clear),
                ..Default::default()
            }
        )
        .await
        .expect("B3 update store operation")
        .is_none(),
        "B3"
    );
}

pub async fn scope_round_trips_and_updates<R: EnvRegistry>(r: &R) {
    let item = r
        .create_scoped(
            "scoped".into(),
            String::new(),
            Default::default(),
            Some("organization".into()),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .expect("create scoped Environment");
    assert_eq!(
        r.get(&item.id)
            .await
            .expect("read scoped Environment")
            .unwrap()
            .scope
            .as_deref(),
        Some("organization")
    );
    let updated = r
        .update(
            &item.id,
            item.revision,
            EnvUpdate {
                scope: Some(EnvironmentFieldUpdate::Replace("account".into())),
                ..Default::default()
            },
        )
        .await
        .expect("update scoped Environment store operation")
        .unwrap();
    assert_eq!(updated.scope.as_deref(), Some("account"));
    assert_eq!(updated.revision, EnvironmentRevision(2));
}

pub async fn nested_config_patch_is_atomic_and_durable<R: EnvRegistry>(r: &R) {
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
        .await
        .expect("create patch Environment");
    r.update(
        &item.id,
        item.revision,
        EnvUpdate {
            config: Some(EnvironmentConfigMutation::PatchCloud {
                networking: Some(EnvironmentNetworkingMutation::Limited {
                    allowed_hosts: None,
                    allow_mcp_servers: Some(EnvironmentFieldUpdate::Replace(false)),
                    allow_package_managers: None,
                }),
                packages: Some(EnvironmentPackagesMutation {
                    npm: Some(EnvironmentFieldUpdate::Clear),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        },
    )
    .await
    .expect("atomic patch store operation")
    .expect("atomic patch");
    let reread = r
        .get(&item.id)
        .await
        .expect("durable read store operation")
        .expect("durable reread");
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

pub async fn package_network_invariant_is_atomic<R: EnvRegistry>(r: &R) {
    // Cause/effect graph for the only cross-field Environment invariant:
    // C1 packages configured; C2 limited networking; C3 package-manager access;
    // C4 one atomic patch changes both package and network fields. E1 accepts a
    // reachable package plan; E2 rejects an unreachable plan; E3 leaves current
    // state, revision history, and delivery intent unchanged on rejection.
    //
    // | Rule | packages | networking | manager access | effect |
    // | I1 | empty    | limited | false | accept |
    // | I2 | nonempty | limited | false | reject, persist nothing |
    // | I3 | nonempty | limited | true  | accept |
    // | I4 | add      | limited | false | reject update atomically (E3) |
    // | I5 | clear    | limited | false | accept atomic two-field update |
    // | I6 | add      | limited | true  | accept atomic two-field update |
    let limited = |allow_package_managers, packages| EnvironmentConfig::Cloud {
        networking: EnvironmentNetworking::Limited {
            allowed_hosts: Vec::new(),
            allow_mcp_servers: false,
            allow_package_managers,
        },
        packages,
    };
    let packages = || EnvironmentPackages {
        npm: vec!["tsx".into()],
        ..Default::default()
    };

    let empty = r
        .create(
            "I1".into(),
            String::new(),
            Default::default(),
            limited(false, EnvironmentPackages::default()),
        )
        .await
        .expect("I1 empty packages do not need package-manager access");

    let invalid = CreateEnvironmentCommand {
        command_id: "invalid-package-network".into(),
        name: "I2".into(),
        description: None,
        metadata: Default::default(),
        scope: None,
        config: limited(false, packages()),
        sandbox_policy: None,
    };
    assert_eq!(
        r.create_once(invalid).await.unwrap_err(),
        CreateEnvironmentError::InvalidConfig(
            InvalidEnvironmentConfig::PackagesRequirePackageManager
        ),
        "I2"
    );
    assert_eq!(r.list_active().await.unwrap().len(), 1, "I2 no row");

    let configured = r
        .create(
            "I3".into(),
            String::new(),
            Default::default(),
            limited(true, packages()),
        )
        .await
        .expect("I3");
    let before = configured.clone();
    let before_intents = r
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .unwrap();
    assert_eq!(
        r.update(
            &configured.id,
            configured.revision,
            EnvUpdate {
                config: Some(EnvironmentConfigMutation::PatchCloud {
                    networking: Some(EnvironmentNetworkingMutation::Limited {
                        allowed_hosts: None,
                        allow_mcp_servers: None,
                        allow_package_managers: Some(EnvironmentFieldUpdate::Replace(false)),
                    }),
                    packages: None,
                }),
                ..Default::default()
            }
        )
        .await
        .unwrap_err(),
        EnvironmentStoreError::InvalidConfig(
            InvalidEnvironmentConfig::PackagesRequirePackageManager
        ),
        "I4"
    );
    assert_eq!(
        r.get(&configured.id).await.unwrap(),
        Some(before.clone()),
        "I4"
    );
    assert_eq!(
        r.registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap(),
        before_intents,
        "I4 no delivery intent"
    );
    assert!(
        r.get_revision(&configured.id, EnvironmentRevision(2))
            .await
            .unwrap()
            .is_none(),
        "I4 no revision"
    );

    let cleared = r
        .update(
            &configured.id,
            configured.revision,
            EnvUpdate {
                config: Some(EnvironmentConfigMutation::PatchCloud {
                    networking: Some(EnvironmentNetworkingMutation::Limited {
                        allowed_hosts: None,
                        allow_mcp_servers: None,
                        allow_package_managers: Some(EnvironmentFieldUpdate::Replace(false)),
                    }),
                    packages: Some(EnvironmentPackagesMutation {
                        reset: true,
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("I5 store")
        .expect("I5 row");
    assert_eq!(cleared.revision, EnvironmentRevision(2), "I5");
    cleared.config.validate().expect("I5 final state");

    let enabled = r
        .update(
            &empty.id,
            empty.revision,
            EnvUpdate {
                config: Some(EnvironmentConfigMutation::PatchCloud {
                    networking: Some(EnvironmentNetworkingMutation::Limited {
                        allowed_hosts: None,
                        allow_mcp_servers: None,
                        allow_package_managers: Some(EnvironmentFieldUpdate::Replace(true)),
                    }),
                    packages: Some(EnvironmentPackagesMutation {
                        npm: Some(EnvironmentFieldUpdate::Replace(vec!["tsx".into()])),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("I6 store")
        .expect("I6 row");
    assert_eq!(enabled.revision, EnvironmentRevision(2), "I6");
    enabled.config.validate().expect("I6 final state");
}

pub async fn idempotent_create_decision_table<R: EnvRegistry>(r: &R) {
    // Cause/effect graph: a new command creates one row; the same id and exact
    // payload replays that row; the same id with another payload conflicts and
    // creates no row. This runs unchanged against memory and SQLite.
    let command = CreateEnvironmentCommand {
        command_id: "command-1".into(),
        name: "stable".into(),
        description: None,
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
        sandbox_policy: None,
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
    assert_eq!(
        r.list_active()
            .await
            .expect("list active Environments")
            .len(),
        1,
        "R1-R3 one effect"
    );
}

/// Every Environment instant is a reading of the registry's injected clock.
///
/// Decision table. Causes: C1 a create; C2 an update that changes something;
/// C3 an update that changes nothing; C4 an archive; C5 a second archive of the
/// same record. Effects: E1 `created_at` and `updated_at` are the reading at the
/// create; E2 `updated_at` advances to the reading at the commit while
/// `created_at` does not move; E3 a no-op update commits nothing, so neither
/// moves; E4 `archived_at` and `updated_at` are the reading at the archive;
/// E5 the replayed archive is idempotent and moves nothing. Rules R1..R5.
///
/// Diffusion: a sibling Environment created at a different instant, untouched by
/// any of these commands, keeps its own pair — so a fix applied to one path only
/// is caught. The immutable revision history is checked too: each authored
/// revision carries the instants of the commit that authored it.
pub async fn environment_instants_are_the_injected_clock<R: EnvRegistry>(
    r: &R,
    clock: &ManualClock,
) {
    const CREATED: u64 = 1_700_000_000_000;
    const SIBLING: u64 = CREATED + 1_000;
    const UPDATED: u64 = CREATED + 7_000;
    const NOOP: u64 = CREATED + 11_000;
    const ARCHIVED: u64 = CREATED + 23_000;

    clock.set(CREATED);
    let id = make(r, "instants").await;
    clock.set(SIBLING);
    let sibling_id = make(r, "sibling").await;

    let created = r.get(&id).await.expect("R1").expect("R1 record");
    assert_eq!(
        created.created_at,
        Some(epoch_millis_to_rfc3339(CREATED)),
        "R1/E1"
    );
    assert_eq!(
        created.updated_at,
        Some(epoch_millis_to_rfc3339(CREATED)),
        "R1/E1"
    );
    assert_eq!(created.archived_at, None, "R1 not archived");

    clock.set(UPDATED);
    let updated = r
        .update(
            &id,
            created.revision,
            EnvUpdate {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .expect("R2")
        .expect("R2 record");
    assert_eq!(
        updated.created_at, created.created_at,
        "R2/E2 acceptance is fixed"
    );
    assert_eq!(
        updated.updated_at,
        Some(epoch_millis_to_rfc3339(UPDATED)),
        "R2/E2"
    );

    clock.set(NOOP);
    let unchanged = r
        .update(&id, updated.revision, EnvUpdate::default())
        .await
        .expect("R3")
        .expect("R3 record");
    assert_eq!(
        unchanged.updated_at, updated.updated_at,
        "R3/E3 no commit, no instant"
    );
    assert_eq!(
        unchanged.revision, updated.revision,
        "R3/E3 no revision either"
    );

    clock.set(ARCHIVED);
    let archived = r.archive(&id).await.expect("R4").expect("R4 record");
    let at_archive = Some(epoch_millis_to_rfc3339(ARCHIVED));
    assert_eq!(archived.archived_at, at_archive, "R4/E4");
    assert_eq!(
        archived.updated_at, at_archive,
        "R4/E4 one reading, both fields"
    );
    assert_eq!(
        archived.created_at, created.created_at,
        "R4/E4 acceptance is fixed"
    );

    clock.set(ARCHIVED + 60_000);
    let replayed = r.archive(&id).await.expect("R5").expect("R5 record");
    assert_eq!(replayed.archived_at, at_archive, "R5/E5 idempotent");
    assert_eq!(replayed.updated_at, at_archive, "R5/E5 idempotent");

    // Diffusion over the sibling object in the same store.
    let sibling = r
        .get(&sibling_id)
        .await
        .expect("sibling")
        .expect("sibling record");
    assert_eq!(
        sibling.created_at,
        Some(epoch_millis_to_rfc3339(SIBLING)),
        "diffusion"
    );
    assert_eq!(
        sibling.updated_at,
        Some(epoch_millis_to_rfc3339(SIBLING)),
        "diffusion"
    );
    assert_eq!(sibling.archived_at, None, "diffusion: untouched");

    // Immutable history carries the instants of the commit that authored it.
    let first = r
        .get_revision(&id, EnvironmentRevision(1))
        .await
        .expect("history")
        .expect("revision 1");
    assert_eq!(
        first.updated_at,
        Some(epoch_millis_to_rfc3339(CREATED)),
        "history R1"
    );
    let second = r
        .get_revision(&id, updated.revision)
        .await
        .expect("history")
        .expect("revision 2");
    assert_eq!(
        second.updated_at,
        Some(epoch_millis_to_rfc3339(UPDATED)),
        "history R2"
    );
}

pub async fn run_suite<R: EnvRegistry>(fresh_with: impl Fn(Arc<dyn WallClock>) -> R) {
    let fresh = || fresh_with(system_clock());
    unique_ids(&fresh()).await;
    archive_preserves_authoritative_history(&fresh()).await;
    missing_id_fails_closed(&fresh()).await;
    revision_decision_table(&fresh()).await;
    scope_round_trips_and_updates(&fresh()).await;
    nested_config_patch_is_atomic_and_durable(&fresh()).await;
    package_network_invariant_is_atomic(&fresh()).await;
    idempotent_create_decision_table(&fresh()).await;
    sandbox_binding_is_one_environment_revision(&fresh()).await;
    let clock = Arc::new(ManualClock::new(0));
    let registry = fresh_with(clock.clone());
    environment_instants_are_the_injected_clock(&registry, &clock).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────
