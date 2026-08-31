//! Deployment-owned local and PostgreSQL installation identity fencing.
//!
//! Component stores retain their existing scoped ledgers and domain ownership.
//! This CLI-private adapter proves local marker/Session continuity and that every
//! effective PostgreSQL target belongs to the deployment's configured platform
//! Workspace before a service opens it or a migration applies component DDL.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use awaken_scoped_migration::postgres::PostgresMigrationRunner;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::config::{OperatingMode, ResolvedDeployment, ResourceStoreBackend};
use crate::process_stores::{role_owns_control_component, role_owns_managed_execution};

const BUNDLE_ID: &str = "awaken.installation_binding";
const PREFIX: &str = "awaken_installation";
const BINDING_TABLE: &str = "awaken_installation_binding";
const LEDGER_TABLE: &str = "awaken_installation_schema_migrations";
const LEDGER_META_TABLE: &str = "awaken_installation_schema_migrations_meta";
const BINDING_LOCK: &str = "installation-binding-v1";

fn installation_binding_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "installation binding",
            include_str!("migrations/V0001__installation_binding.sql"),
        )?],
    )
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct InstallationAuthorization {
    initialization_reference: Option<String>,
    adoption_reference: Option<String>,
}

impl InstallationAuthorization {
    pub(crate) fn new(
        initialization_reference: Option<String>,
        adoption_reference: Option<String>,
    ) -> Result<Self, String> {
        for (flag, reference) in [
            (
                "--initialization-reference",
                initialization_reference.as_deref(),
            ),
            ("--adoption-reference", adoption_reference.as_deref()),
        ] {
            if reference.is_some_and(|value| value.trim() != value || value.is_empty()) {
                return Err(format!(
                    "{flag} must be non-empty and have no surrounding whitespace"
                ));
            }
        }
        Ok(Self {
            initialization_reference,
            adoption_reference,
        })
    }

    fn permits_initialization(&self) -> bool {
        self.initialization_reference.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreparedLocalInstallation {
    NotOwned,
    Existing {
        root: std::path::PathBuf,
        workspace_id: String,
    },
    AuthorizedMissing {
        root: std::path::PathBuf,
        expected_workspace_id: Option<String>,
    },
}

impl PreparedLocalInstallation {
    fn owns_local_persistence(&self) -> bool {
        !matches!(self, Self::NotOwned)
    }

    /// Revalidate only the exact observation that produced this proof. This is
    /// deliberately not another classifier: an existing A can neither become B
    /// nor degrade into the missing-marker initialization path between preflight
    /// and the first store write.
    fn workspace_before_write(&self) -> Result<Option<String>, String> {
        let (root, expected, missing_was_authorized) = match self {
            Self::NotOwned => return Ok(None),
            Self::Existing { root, workspace_id } => (root, Some(workspace_id.as_str()), false),
            Self::AuthorizedMissing {
                root,
                expected_workspace_id,
            } => (root, expected_workspace_id.as_deref(), true),
        };
        let current =
            awaken_runtime_host::SharedHost::local_workspace_at(root).map_err(|error| {
                format!(
                    "platform_workspace_unavailable: inspect identity under {}: {error}",
                    root.display()
                )
            })?;
        match (expected, current, missing_was_authorized) {
            (Some(expected), Some(actual), _) if actual == expected => Ok(Some(actual)),
            (Some(expected), Some(actual), _) => Err(format!(
                "local_installation_changed: expected platform-workspace-id {expected:?}, found {actual:?} under {} after admission",
                root.display()
            )),
            (Some(expected), None, true) => Ok(Some(expected.to_owned())),
            (Some(expected), None, false) => Err(format!(
                "local_installation_changed: platform-workspace-id {expected:?} disappeared under {} after admission",
                root.display()
            )),
            (None, Some(actual), true) => Ok(Some(actual)),
            (None, None, true) => Ok(None),
            (None, _, false) => unreachable!("an existing local proof always carries its identity"),
        }
    }
}

#[derive(Debug)]
struct PreparedPostgresInstallations {
    target_count: usize,
    workspace_id: Option<String>,
}

impl PreparedPostgresInstallations {
    fn target_count(&self) -> usize {
        self.target_count
    }
}

/// Proof that the one deployment-installation preflight completed before any
/// role-owned store or migration opens. It retains the exact local observation
/// and the already-validated PostgreSQL coordinate; callers may not reconstruct
/// either value from mutable deployment state.
pub(crate) struct PreparedDeploymentInstallations {
    local: PreparedLocalInstallation,
    postgres: PreparedPostgresInstallations,
}

impl PreparedDeploymentInstallations {
    pub(crate) fn postgres_target_count(&self) -> usize {
        self.postgres.target_count()
    }

    pub(crate) fn publishes_local_workspace(&self) -> bool {
        self.local.owns_local_persistence()
    }

    /// Exact process coordinate after the final local fence and before the first
    /// role-owned store write. Mixed local/PostgreSQL deployments must prove both
    /// authorities resolve to the same coordinate.
    pub(crate) fn platform_workspace_before_write(&self) -> Result<Option<String>, String> {
        let local = self.local.workspace_before_write()?;
        match (&self.postgres.workspace_id, local) {
            (Some(expected), Some(actual)) if actual != *expected => Err(format!(
                "local_installation_changed: PostgreSQL expects platform Workspace {expected:?}, but the exact local fence returned {actual:?}"
            )),
            (Some(expected), _) => Ok(Some(expected.clone())),
            (None, local) => Ok(local),
        }
    }
}

/// Read-only Serve/Doctor admission. Both consumers receive the same typed
/// proof; only Serve invokes its before-write fence.
pub(crate) async fn verify_deployment_installations(
    deployment: &ResolvedDeployment,
) -> Result<PreparedDeploymentInstallations, String> {
    let local = classify_local_installation(deployment, &InstallationAuthorization::default())?;
    let postgres = verify_deployment_postgres_installations(deployment).await?;
    Ok(PreparedDeploymentInstallations { local, postgres })
}

/// Preflight every local and PostgreSQL installation target under one explicit
/// authorization. Local classification is read-only and runs before the PG
/// binder, so an invalid local authority cannot leave a new remote binding.
pub(crate) async fn prepare_deployment_installations(
    deployment: &ResolvedDeployment,
    authorization: &InstallationAuthorization,
) -> Result<PreparedDeploymentInstallations, String> {
    let local = classify_local_installation(deployment, authorization)?;
    local.workspace_before_write()?;
    let postgres = prepare_deployment_postgres_installations(deployment, authorization).await?;
    Ok(PreparedDeploymentInstallations { local, postgres })
}

fn classify_local_installation(
    deployment: &ResolvedDeployment,
    authorization: &InstallationAuthorization,
) -> Result<PreparedLocalInstallation, String> {
    if !has_role_owned_local_persistence(deployment) {
        return Ok(PreparedLocalInstallation::NotOwned);
    }

    let marker = awaken_runtime_host::SharedHost::local_workspace_at(&deployment.data_dir)
        .map_err(|error| {
            format!(
                "platform_workspace_unavailable: inspect identity under {}: {error}",
                deployment.data_dir.display()
            )
        })?;
    if let Some(expected) = deployment.expected_platform_workspace_id.as_deref() {
        match marker.as_deref() {
            None if authorization.permits_initialization() => {}
            None => {
                return Err(format!(
                    "platform_workspace_missing: expected {expected:?} under {}",
                    deployment.data_dir.display()
                ));
            }
            Some(actual) if actual != expected => {
                return Err(format!(
                    "platform_workspace_mismatch: expected {expected:?}, found {actual:?} under {}",
                    deployment.data_dir.display()
                ));
            }
            Some(_) => {}
        }
    }

    let session_path = match (deployment.role, &deployment.coordinator.sessions) {
        (
            crate::config::Role::AllInOne | crate::config::Role::Coordinator,
            awaken_control::StoreBackend::Sqlite(path),
        ) => Some(path),
        _ => None,
    };
    let Some(session_path) = session_path else {
        return match marker {
            Some(workspace_id) => Ok(PreparedLocalInstallation::Existing {
                root: deployment.data_dir.clone(),
                workspace_id,
            }),
            None if authorization.permits_initialization() => {
                Ok(PreparedLocalInstallation::AuthorizedMissing {
                    root: deployment.data_dir.clone(),
                    expected_workspace_id: deployment.expected_platform_workspace_id.clone(),
                })
            }
            None => Err(format!(
                "unbound_empty_local_storage: ordinary startup and database migrate are exact-only; first installation requires explicit database migrate --initialize-installation --initialization-reference <REF> under {}",
                deployment.data_dir.display()
            )),
        };
    };
    let session_exists = match std::fs::metadata(session_path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "session_storage_invalid: {} is not a regular file",
                session_path.display()
            ));
        }
        Ok(metadata) if metadata.len() == 0 => {
            return Err(format!(
                "session_storage_empty: {} is zero bytes",
                session_path.display()
            ));
        }
        Ok(_) => {
            awaken_session_store::SqliteManagedSessionRepository::verify_existing(
                &session_path.to_string_lossy(),
            )
            .map_err(|error| {
                format!(
                    "session_storage_invalid: verify {}: {error}",
                    session_path.display()
                )
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "session_storage_unavailable: inspect {}: {error}",
                session_path.display()
            ));
        }
    };

    match (marker.as_ref(), session_exists) {
        (Some(workspace_id), true) => Ok(PreparedLocalInstallation::Existing {
            root: deployment.data_dir.clone(),
            workspace_id: workspace_id.clone(),
        }),
        (Some(workspace_id), false) if authorization.permits_initialization() => {
            Ok(PreparedLocalInstallation::Existing {
                root: deployment.data_dir.clone(),
                workspace_id: workspace_id.clone(),
            })
        }
        (Some(_), false) => Err(format!(
            "session_storage_missing: installation under {} is missing {}; explicit --initialize-installation --initialization-reference <REF> is required for role-first completion or intentional recovery",
            deployment.data_dir.display(),
            session_path.display()
        )),
        (None, true) if authorization.permits_initialization() => {
            Ok(PreparedLocalInstallation::AuthorizedMissing {
                root: deployment.data_dir.clone(),
                expected_workspace_id: deployment.expected_platform_workspace_id.clone(),
            })
        }
        (None, true) => Err(format!(
            "local_installation_incomplete: canonical Session storage exists under {}, but platform-workspace-id is missing; retry only with explicit --initialize-installation --initialization-reference <REF>",
            deployment.data_dir.display()
        )),
        (None, false) if authorization.permits_initialization() => {
            Ok(PreparedLocalInstallation::AuthorizedMissing {
                root: deployment.data_dir.clone(),
                expected_workspace_id: deployment.expected_platform_workspace_id.clone(),
            })
        }
        (None, false) => Err(format!(
            "unbound_empty_local_storage: ordinary startup and database migrate are exact-only; first installation requires explicit database migrate --initialize-installation --initialization-reference <REF> under {}",
            deployment.data_dir.display()
        )),
    }
}

#[derive(Debug)]
struct InstallationTarget {
    labels: Vec<&'static str>,
    url: String,
}

#[derive(Debug)]
struct PreparedTarget {
    labels: Vec<&'static str>,
    pool: PgPool,
    action: BindingAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingAction {
    Bound,
    InitializeFresh,
    AdoptLegacy,
}

#[derive(Debug, Eq, PartialEq)]
struct BindingRow {
    platform_workspace_id: String,
    binding_origin: String,
    operator_reference: String,
}

#[derive(Debug)]
enum BundleObservation {
    Missing,
    Current(Option<BindingRow>),
    Invalid(String),
}

#[derive(Debug)]
struct TargetObservation {
    objects: BTreeSet<String>,
    bundle: BundleObservation,
}

/// Read-only admission used by Serve and Doctor. It never bootstraps a scoped
/// ledger, creates a binding row, or applies component schema.
async fn verify_deployment_postgres_installations(
    deployment: &ResolvedDeployment,
) -> Result<PreparedPostgresInstallations, String> {
    let targets = postgres_targets(deployment);
    let expected = required_workspace_id(deployment, targets.is_empty())?;
    let Some(expected) = expected else {
        return Ok(PreparedPostgresInstallations {
            target_count: 0,
            workspace_id: None,
        });
    };
    for target in &targets {
        let pool = connect_target(target).await?;
        let observation = observe_target(&pool)
            .await
            .map_err(|error| target_error(target.labels.as_slice(), error))?;
        classify_observation(
            &observation,
            expected,
            &InstallationAuthorization::default(),
        )
        .map_err(|error| target_error(target.labels.as_slice(), error))?
        .then_bound()
        .map_err(|error| target_error(target.labels.as_slice(), error))?;
        pool.close().await;
    }
    Ok(PreparedPostgresInstallations {
        target_count: targets.len(),
        workspace_id: Some(expected.to_owned()),
    })
}

/// Preflight every effective PostgreSQL target before writing any binding or
/// invoking the existing component migration manifest. A failure therefore
/// leaves every target untouched; a later per-target bind failure may leave
/// only exact bindings that are safe to replay.
async fn prepare_deployment_postgres_installations(
    deployment: &ResolvedDeployment,
    authorization: &InstallationAuthorization,
) -> Result<PreparedPostgresInstallations, String> {
    let targets = postgres_targets(deployment);
    let expected = required_workspace_id(deployment, targets.is_empty())?;
    let Some(expected) = expected else {
        return Ok(PreparedPostgresInstallations {
            target_count: 0,
            workspace_id: None,
        });
    };

    let mut prepared = Vec::with_capacity(targets.len());
    for target in targets {
        let pool = connect_target(&target).await?;
        let observation = observe_target(&pool)
            .await
            .map_err(|error| target_error(target.labels.as_slice(), error))?;
        let action = classify_observation(&observation, expected, authorization)
            .map_err(|error| target_error(target.labels.as_slice(), error))?;
        prepared.push(PreparedTarget {
            labels: target.labels,
            pool,
            action,
        });
    }

    let count = prepared.len();
    for target in prepared {
        if target.action != BindingAction::Bound {
            bind_target(
                &target.pool,
                expected,
                target.action,
                authorization,
                target.labels.as_slice(),
            )
            .await?;
        }
        target.pool.close().await;
    }
    Ok(PreparedPostgresInstallations {
        target_count: count,
        workspace_id: Some(expected.to_owned()),
    })
}

fn required_workspace_id(
    deployment: &ResolvedDeployment,
    no_postgres_targets: bool,
) -> Result<Option<&str>, String> {
    match deployment.expected_platform_workspace_id.as_deref() {
        Some(expected) if !expected.is_empty() && expected.trim() == expected => Ok(Some(expected)),
        Some(_) => Err("expected_platform_workspace_id must be a non-empty exact value".into()),
        None if deployment.mode == OperatingMode::Server || !no_postgres_targets => Err(
            "expected_platform_workspace_id_required: Server and PostgreSQL deployments must configure expected_platform_workspace_id"
                .into(),
        ),
        None => Ok(None),
    }
}

fn postgres_targets(deployment: &ResolvedDeployment) -> Vec<InstallationTarget> {
    let mut targets: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    fn add_backend(
        targets: &mut BTreeMap<String, Vec<&'static str>>,
        label: &'static str,
        backend: &awaken_control::StoreBackend,
    ) {
        if let awaken_control::StoreBackend::Postgres(url) = backend {
            targets.entry(url.clone()).or_default().push(label);
        }
    }
    fn add_url(targets: &mut BTreeMap<String, Vec<&'static str>>, label: &'static str, url: &str) {
        targets.entry(url.to_owned()).or_default().push(label);
    }

    if role_owns_control_component(deployment.role) {
        add_backend(&mut targets, "control.catalog", &deployment.control.catalog);
        add_backend(
            &mut targets,
            "control.credential",
            &deployment.control.credential,
        );
        add_backend(&mut targets, "control.config", &deployment.control.config);
        add_backend(&mut targets, "control.admin", &deployment.control.admin);
        add_backend(
            &mut targets,
            "control.data_subject",
            &deployment.control.data_subject,
        );
        add_backend(
            &mut targets,
            "control.environment",
            &deployment.control.environment,
        );
    }
    if role_owns_managed_execution(deployment.role) {
        if let Some(url) = deployment.runtime.database_url.as_deref() {
            add_url(&mut targets, "coordinator.runtime", url);
        }
        match &deployment.resources {
            ResourceStoreBackend::Embedded(_) => {}
            ResourceStoreBackend::Postgres(url)
            | ResourceStoreBackend::PostgresObject { url, .. } => {
                add_url(&mut targets, "resources", url);
            }
        }
        add_backend(
            &mut targets,
            "coordinator.sessions",
            &deployment.coordinator.sessions,
        );
        add_backend(
            &mut targets,
            "coordinator.captured_content",
            &deployment.coordinator.captured_content,
        );
    }

    targets
        .into_iter()
        .map(|(url, mut labels)| {
            labels.sort_unstable();
            labels.dedup();
            InstallationTarget { labels, url }
        })
        .collect()
}

/// Local marker continuity remains authoritative only for persistence that is
/// actually rooted in the local data directory.
pub(crate) fn has_role_owned_local_persistence(deployment: &ResolvedDeployment) -> bool {
    let sqlite = |backend: &awaken_control::StoreBackend| {
        matches!(backend, awaken_control::StoreBackend::Sqlite(_))
    };
    let control_local = role_owns_control_component(deployment.role)
        && [
            &deployment.control.catalog,
            &deployment.control.credential,
            &deployment.control.config,
            &deployment.control.admin,
            &deployment.control.data_subject,
            &deployment.control.environment,
        ]
        .into_iter()
        .any(sqlite);
    let coordinator_local = role_owns_managed_execution(deployment.role)
        && (matches!(
            deployment.runtime.store,
            awaken_runtime_host::StoreKind::Sqlite
        ) || matches!(&deployment.resources, ResourceStoreBackend::Embedded(_))
            || sqlite(&deployment.coordinator.sessions)
            || sqlite(&deployment.coordinator.captured_content));
    control_local || coordinator_local
}

async fn connect_target(target: &InstallationTarget) -> Result<PgPool, String> {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&target.url)
        .await
        .map_err(|error| {
            target_error(
                target.labels.as_slice(),
                format!("postgres_installation_unavailable: {error}"),
            )
        })
}

async fn observe_target(pool: &PgPool) -> Result<TargetObservation, String> {
    let objects = schema_objects(pool).await?;
    let runner = PostgresMigrationRunner::with_prefix(pool.clone(), PREFIX)
        .map_err(|error| format!("installation_binding_invalid: {error}"))?;
    let bundle = installation_binding_bundle()
        .map_err(|error| format!("installation_binding_invalid: {error}"))?;
    let bundle = match runner.verify_bundle(&bundle).await {
        Ok(()) => BundleObservation::Current(read_binding_row(pool).await?),
        Err(MigrationError::MissingLedger { .. }) => BundleObservation::Missing,
        Err(error) => BundleObservation::Invalid(error.to_string()),
    };
    Ok(TargetObservation { objects, bundle })
}

async fn schema_objects(pool: &PgPool) -> Result<BTreeSet<String>, String> {
    let rows = sqlx::query(
        "SELECT 'relation:' || c.relname AS object_name \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() \
           AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
         UNION ALL \
         SELECT 'function:' || p.proname || ':' || p.oid::text AS object_name \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = current_schema() \
         ORDER BY object_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("postgres_installation_probe_failed: {error}"))?;
    rows.into_iter()
        .map(|row| {
            row.try_get("object_name")
                .map_err(|error| format!("postgres_installation_probe_failed: {error}"))
        })
        .collect()
}

async fn read_binding_row(pool: &PgPool) -> Result<Option<BindingRow>, String> {
    let rows = sqlx::query(
        "SELECT platform_workspace_id, binding_origin, operator_reference \
         FROM awaken_installation_binding ORDER BY singleton",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("installation_binding_corrupt: read binding row: {error}"))?;
    if rows.len() > 1 {
        return Err(format!(
            "installation_binding_corrupt: expected at most one row, found {}",
            rows.len()
        ));
    }
    rows.into_iter()
        .next()
        .map(|row| {
            Ok(BindingRow {
                platform_workspace_id: row.try_get("platform_workspace_id").map_err(|error| {
                    format!("installation_binding_corrupt: decode Workspace: {error}")
                })?,
                binding_origin: row.try_get("binding_origin").map_err(|error| {
                    format!("installation_binding_corrupt: decode origin: {error}")
                })?,
                operator_reference: row.try_get("operator_reference").map_err(|error| {
                    format!("installation_binding_corrupt: decode operator reference: {error}")
                })?,
            })
        })
        .transpose()
}

fn classify_observation(
    observation: &TargetObservation,
    expected: &str,
    authorization: &InstallationAuthorization,
) -> Result<BindingAction, String> {
    let exact_binding_objects = BTreeSet::from([
        format!("relation:{BINDING_TABLE}"),
        format!("relation:{LEDGER_TABLE}"),
        format!("relation:{LEDGER_META_TABLE}"),
    ]);
    let has_binding_artifact = observation.objects.iter().any(|object| {
        object
            .strip_prefix("relation:")
            .is_some_and(|name| name.starts_with(PREFIX))
    });
    let other_objects = observation
        .objects
        .difference(&exact_binding_objects)
        .next()
        .is_some();

    match &observation.bundle {
        BundleObservation::Invalid(error) => Err(format!(
            "installation_binding_corrupt: scoped ledger verification failed: {error}"
        )),
        BundleObservation::Missing if has_binding_artifact => Err(
            "installation_binding_corrupt: partial or unledgered binding schema".into(),
        ),
        BundleObservation::Missing if !other_objects => authorization
            .initialization_reference
            .as_ref()
            .map_or_else(
                || {
                    Err(
                        "unbound_empty_database: ordinary database migrate is exact-only; first installation requires explicit --initialize-installation --initialization-reference <REF>"
                            .into(),
                    )
                },
                |_| Ok(BindingAction::InitializeFresh),
            ),
        BundleObservation::Missing => authorization.adoption_reference.as_ref().map_or_else(
            || {
                Err(
                    "unbound_existing_database: rerun database migrate only with explicit --adopt-unbound-existing --adoption-reference <REF> after operator verification"
                        .into(),
                )
            },
            |_| Ok(BindingAction::AdoptLegacy),
        ),
        BundleObservation::Current(Some(row)) => verify_binding_row(row, expected),
        BundleObservation::Current(None) if !other_objects => authorization
            .initialization_reference
            .as_ref()
            .map_or_else(
                || {
                    Err(
                        "unbound_empty_database: binding schema has no identity row; retry requires the explicit initialization authorization"
                            .into(),
                    )
                },
                |_| Ok(BindingAction::InitializeFresh),
            ),
        BundleObservation::Current(None) => authorization.adoption_reference.as_ref().map_or_else(
            || {
                Err(
                    "unbound_existing_database: binding schema has no identity row; explicit legacy adoption is required"
                        .into(),
                )
            },
            |_| Ok(BindingAction::AdoptLegacy),
        ),
    }
}

fn verify_binding_row(row: &BindingRow, expected: &str) -> Result<BindingAction, String> {
    if row.platform_workspace_id != expected {
        return Err(format!(
            "postgres_installation_mismatch: expected platform Workspace {expected:?}, found {:?}",
            row.platform_workspace_id
        ));
    }
    let origin_valid = matches!(
        row.binding_origin.as_str(),
        "fresh_initialization" | "legacy_adoption"
    ) && !row.operator_reference.is_empty()
        && row.operator_reference.trim() == row.operator_reference;
    if !origin_valid {
        return Err("installation_binding_corrupt: invalid origin/reference pair".into());
    }
    Ok(BindingAction::Bound)
}

async fn bind_target(
    pool: &PgPool,
    expected: &str,
    action: BindingAction,
    authorization: &InstallationAuthorization,
    labels: &[&str],
) -> Result<(), String> {
    let runner = PostgresMigrationRunner::with_prefix(pool.clone(), PREFIX)
        .map_err(|error| target_error(labels, format!("installation_binding_invalid: {error}")))?;
    let bundle = installation_binding_bundle()
        .map_err(|error| target_error(labels, format!("installation_binding_invalid: {error}")))?;
    runner.run_bundle(&bundle).await.map_err(|error| {
        target_error(
            labels,
            format!("installation_binding_apply_failed: {error}"),
        )
    })?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| target_error(labels, format!("installation_binding_begin: {error}")))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(BINDING_TABLE)
        .bind(BINDING_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(|error| target_error(labels, format!("installation_binding_lock: {error}")))?;
    let existing = sqlx::query(
        "SELECT platform_workspace_id, binding_origin, operator_reference \
         FROM awaken_installation_binding WHERE singleton = 1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| target_error(labels, format!("installation_binding_read: {error}")))?;
    if let Some(row) = existing {
        let row = BindingRow {
            platform_workspace_id: row.try_get("platform_workspace_id").map_err(|error| {
                target_error(labels, format!("installation_binding_decode: {error}"))
            })?,
            binding_origin: row.try_get("binding_origin").map_err(|error| {
                target_error(labels, format!("installation_binding_decode: {error}"))
            })?,
            operator_reference: row.try_get("operator_reference").map_err(|error| {
                target_error(labels, format!("installation_binding_decode: {error}"))
            })?,
        };
        verify_binding_row(&row, expected).map_err(|error| target_error(labels, error))?;
        tx.rollback().await.map_err(|error| {
            target_error(labels, format!("installation_binding_rollback: {error}"))
        })?;
        return Ok(());
    }

    if action == BindingAction::InitializeFresh {
        let rows = sqlx::query(
            "SELECT 'relation:' || c.relname AS object_name \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() \
               AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
             UNION ALL \
             SELECT 'function:' || p.proname || ':' || p.oid::text AS object_name \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = current_schema()",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| {
            target_error(
                labels,
                format!("postgres_installation_probe_failed: {error}"),
            )
        })?;
        let allowed = BTreeSet::from([
            format!("relation:{BINDING_TABLE}"),
            format!("relation:{LEDGER_TABLE}"),
            format!("relation:{LEDGER_META_TABLE}"),
        ]);
        let mut raced = false;
        for row in rows {
            let object: String = row.try_get("object_name").map_err(|error| {
                target_error(
                    labels,
                    format!("postgres_installation_probe_failed: {error}"),
                )
            })?;
            raced |= !allowed.contains(&object);
        }
        if raced {
            tx.rollback().await.map_err(|error| {
                target_error(labels, format!("installation_binding_rollback: {error}"))
            })?;
            return Err(target_error(
                labels,
                "unbound_existing_database: schema changed after fresh preflight; explicit operator adoption is required",
            ));
        }
    }

    let (origin, reference) = match action {
        BindingAction::InitializeFresh => (
            "fresh_initialization",
            authorization
                .initialization_reference
                .as_deref()
                .expect("initialization action requires explicit authorization"),
        ),
        BindingAction::AdoptLegacy => (
            "legacy_adoption",
            authorization
                .adoption_reference
                .as_deref()
                .expect("legacy action requires explicit authorization"),
        ),
        BindingAction::Bound => unreachable!("bound targets are not written"),
    };
    sqlx::query(
        "INSERT INTO awaken_installation_binding \
         (singleton, platform_workspace_id, binding_origin, operator_reference) \
         VALUES (1, $1, $2, $3)",
    )
    .bind(expected)
    .bind(origin)
    .bind(reference)
    .execute(&mut *tx)
    .await
    .map_err(|error| target_error(labels, format!("installation_binding_insert: {error}")))?;
    tx.commit()
        .await
        .map_err(|error| target_error(labels, format!("installation_binding_commit: {error}")))
}

impl BindingAction {
    fn then_bound(self) -> Result<(), String> {
        match self {
            Self::Bound => Ok(()),
            Self::InitializeFresh => Err(
                "postgres_installation_unbound: first installation requires explicit initialization"
                    .into(),
            ),
            Self::AdoptLegacy => {
                Err("unbound_existing_database: explicit migration adoption is required".into())
            }
        }
    }
}

fn target_error(labels: &[&str], error: impl AsRef<str>) -> String {
    format!(
        "postgres target [{}]: {}",
        labels.join(", "),
        error.as_ref()
    )
}

#[cfg(test)]
mod tests;
