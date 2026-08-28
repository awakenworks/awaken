//! Anti-corruption boundary for Cloud-owned Workspace data authority.
//!
//! The stable manifest identifies durable data independently from a Cell. The
//! time-bounded access lease binds one exact runtime incarnation to that data.
//! Awaken consumes both facts but cannot provision, move, or promote either.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_persistence::{ObjectBackingConfig, ObjectBackingProvider};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

const CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct Coordinate(String);

impl Coordinate {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Coordinate {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err("Workspace data coordinate must be non-empty".into());
        }
        Ok(Self(value))
    }
}

/// Runtime-owned expectation. Durable data coordinates are deliberately not
/// repeated here: the manifest and lease must agree with each other.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    manifest_file: PathBuf,
    access_lease_file: PathBuf,
    workspace_id: Coordinate,
    runtime_region_id: Coordinate,
    runtime_cell_id: Coordinate,
    runtime_cell_incarnation: Coordinate,
    runtime_placement_epoch: u64,
}

/// Request-time authority for a Cloud-hosted Awaken Workspace.
///
/// The guard retains only file coordinates and the expected runtime identity.
/// It rereads the deployment-owned lease for every admitted request, so a
/// revoke, expiry, epoch rotation, or atomic file replacement takes effect
/// without trusting a process-start snapshot.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceDataLeaseGuard(Config);

impl WorkspaceDataLeaseGuard {
    pub(crate) fn validate_now(&self) -> Result<(), String> {
        load(&self.0).map(drop)
    }
}

pub(crate) async fn enforce_workspace_data_lease(
    State(guard): State<WorkspaceDataLeaseGuard>,
    request: Request,
    next: Next,
) -> Response {
    if guard.validate_now().is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(request).await
}

pub(super) fn lease_guard(config: Option<&Config>) -> Option<WorkspaceDataLeaseGuard> {
    config.cloned().map(WorkspaceDataLeaseGuard)
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.manifest_file.as_os_str().is_empty() {
            return Err("workspace_data.manifest_file must not be empty".into());
        }
        if self.access_lease_file.as_os_str().is_empty() {
            return Err("workspace_data.access_lease_file must not be empty".into());
        }
        if self.runtime_placement_epoch == 0 {
            return Err("workspace_data.runtime_placement_epoch must be positive".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductWorkspaceId {
    product: AwakenProduct,
    workspace_id: Coordinate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AwakenProduct {
    Awaken,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDataBackingManifest {
    contract_version: u32,
    backing_id: Coordinate,
    workspace: ProductWorkspaceId,
    data_shard_id: Coordinate,
    data_authority_epoch: u64,
    manifest_revision: u64,
    resources: AwakenDataResources,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AwakenDataResources {
    resources_database: DurableDatabase,
    files: DurableObjectStore,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableDatabase {
    database_ref: Coordinate,
    schema_ref: Coordinate,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableObjectStore {
    protocol: Protocol,
    bucket_ref: Coordinate,
    prefix_ref: Coordinate,
    encryption_key_ref: Coordinate,
    region: Option<String>,
    endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Protocol {
    S3,
    Gcs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDataAccessLease {
    contract_version: u32,
    lease_id: Coordinate,
    workspace: ProductWorkspaceId,
    backing_id: Coordinate,
    data_shard_id: Coordinate,
    runtime: CellRuntimeIdentity,
    data_authority_epoch: u64,
    runtime_placement_epoch: u64,
    valid_from_ms: u64,
    expires_at_ms: u64,
    state: LeaseState,
    access: AwakenDataAccess,
    aggregate_version: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellRuntimeIdentity {
    region_id: Coordinate,
    cell_id: Coordinate,
    cell_incarnation: Coordinate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LeaseState {
    Granted,
    Revoked,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AwakenDataAccess {
    resources_database: DatabaseAccess,
    files: ObjectAccess,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseAccess {
    principal_ref: Coordinate,
    connection_secret_ref: Coordinate,
    connection_secret_key: Coordinate,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectAccess {
    identity_ref: Coordinate,
}

fn read_contract<T: for<'de> Deserialize<'de>>(path: &PathBuf, name: &str) -> Result<T, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read workspace_data.{name} {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse workspace_data.{name} {}: {error}", path.display()))
}

fn positive(value: u64, name: &str) -> Result<(), String> {
    if value == 0 {
        return Err(format!("workspace data {name} must be positive"));
    }
    Ok(())
}

fn load_at(expected: &Config, now_ms: u64) -> Result<ObjectBackingConfig, String> {
    expected.validate()?;
    let manifest: WorkspaceDataBackingManifest =
        read_contract(&expected.manifest_file, "manifest_file")?;
    let lease: WorkspaceDataAccessLease =
        read_contract(&expected.access_lease_file, "access_lease_file")?;

    if manifest.contract_version != CONTRACT_VERSION || lease.contract_version != CONTRACT_VERSION {
        return Err("workspace data contract_version must be 1".into());
    }
    for (value, name) in [
        (
            manifest.data_authority_epoch,
            "manifest data_authority_epoch",
        ),
        (manifest.manifest_revision, "manifest_revision"),
        (lease.data_authority_epoch, "lease data_authority_epoch"),
        (lease.runtime_placement_epoch, "runtime_placement_epoch"),
        (lease.aggregate_version, "aggregate_version"),
    ] {
        positive(value, name)?;
    }
    if manifest.workspace.workspace_id != expected.workspace_id
        || lease.workspace != manifest.workspace
        || lease.backing_id != manifest.backing_id
        || lease.data_shard_id != manifest.data_shard_id
        || lease.data_authority_epoch != manifest.data_authority_epoch
    {
        return Err("Workspace data manifest and access lease authority do not match".into());
    }
    if lease.runtime.region_id != expected.runtime_region_id
        || lease.runtime.cell_id != expected.runtime_cell_id
        || lease.runtime.cell_incarnation != expected.runtime_cell_incarnation
        || lease.runtime_placement_epoch != expected.runtime_placement_epoch
    {
        return Err("Workspace data lease does not authorize this runtime incarnation".into());
    }
    if lease.state != LeaseState::Granted
        || lease.valid_from_ms > now_ms
        || now_ms >= lease.expires_at_ms
    {
        return Err("Workspace data access lease is not currently granted".into());
    }
    if lease.lease_id.as_str().is_empty()
        || manifest
            .resources
            .resources_database
            .database_ref
            .as_str()
            .is_empty()
        || manifest
            .resources
            .resources_database
            .schema_ref
            .as_str()
            .is_empty()
        || lease
            .access
            .resources_database
            .principal_ref
            .as_str()
            .is_empty()
        || lease
            .access
            .resources_database
            .connection_secret_ref
            .as_str()
            .is_empty()
        || lease
            .access
            .resources_database
            .connection_secret_key
            .as_str()
            .is_empty()
        || lease.access.files.identity_ref.as_str().is_empty()
        || manifest
            .resources
            .files
            .encryption_key_ref
            .as_str()
            .is_empty()
    {
        return Err("Workspace data resources or access coordinates are empty".into());
    }

    let object = manifest.resources.files;
    let config = ObjectBackingConfig {
        provider: match object.protocol {
            Protocol::S3 => ObjectBackingProvider::S3,
            Protocol::Gcs => ObjectBackingProvider::Gcs,
        },
        bucket: object.bucket_ref.0,
        prefix: object.prefix_ref.0,
        region: object.region,
        endpoint: object.endpoint,
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

fn load(expected: &Config) -> Result<ObjectBackingConfig, String> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?
        .as_millis()
        .try_into()
        .map_err(|_| "system clock does not fit the Workspace lease clock".to_owned())?;
    load_at(expected, now_ms)
}

pub(super) fn resolve(
    database_url: Option<String>,
    workspace_data: Option<&Config>,
    embedded_root: PathBuf,
) -> Result<super::ResourceStoreBackend, String> {
    let object = workspace_data.map(load).transpose()?;
    match (database_url, object) {
        (Some(url), Some(object)) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::PostgresObject { url, object })
        }
        (Some(url), None) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::Postgres(url))
        }
        (None, Some(_)) => {
            Err("workspace_data requires shared PostgreSQL Resources metadata".into())
        }
        (Some(_), _) => Err("resource_database_url must be postgres://".into()),
        (None, None) => Ok(super::ResourceStoreBackend::Embedded(embedded_root)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt as _;

    fn write(value: serde_json::Value) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        file
    }

    fn manifest() -> serde_json::Value {
        serde_json::json!({
            "contract_version": 1,
            "backing_id": "backing-a",
            "workspace": {"product": "awaken", "workspace_id": "workspace-a"},
            "data_shard_id": "shard-a",
            "data_authority_epoch": 3,
            "manifest_revision": 7,
            "resources": {
                "resources_database": {"database_ref": "databases/a", "schema_ref": "schemas/resources"},
                "files": {"protocol": "gcs", "bucket_ref": "bucket-a", "prefix_ref": "workspaces/a/files", "encryption_key_ref": "keys/a", "region": null, "endpoint": null}
            }
        })
    }

    fn lease() -> serde_json::Value {
        serde_json::json!({
            "contract_version": 1,
            "lease_id": "lease-a",
            "workspace": {"product": "awaken", "workspace_id": "workspace-a"},
            "backing_id": "backing-a",
            "data_shard_id": "shard-a",
            "runtime": {"region_id": "region-a", "cell_id": "cell-a", "cell_incarnation": "incarnation-a"},
            "data_authority_epoch": 3,
            "runtime_placement_epoch": 5,
            "valid_from_ms": 100,
            "expires_at_ms": 4_000_000_000_000_u64,
            "state": "granted",
            "access": {
                "resources_database": {"principal_ref": "principals/a", "connection_secret_ref": "secrets/database", "connection_secret_key": "url"},
                "files": {"identity_ref": "identities/files"}
            },
            "aggregate_version": 2
        })
    }

    fn parse(
        manifest_value: serde_json::Value,
        lease_value: serde_json::Value,
    ) -> Result<ObjectBackingConfig, String> {
        let manifest = write(manifest_value);
        let lease = write(lease_value);
        load_at(
            &Config {
                manifest_file: manifest.path().into(),
                access_lease_file: lease.path().into(),
                workspace_id: "workspace-a".to_owned().try_into().unwrap(),
                runtime_region_id: "region-a".to_owned().try_into().unwrap(),
                runtime_cell_id: "cell-a".to_owned().try_into().unwrap(),
                runtime_cell_incarnation: "incarnation-a".to_owned().try_into().unwrap(),
                runtime_placement_epoch: 5,
            },
            200,
        )
    }

    #[test]
    fn stable_manifest_and_runtime_lease_have_one_joint_authority() {
        // Cause/effect design: C1=closed v1 schemas, C2=Workspace/backing/shard/
        // data epoch agree, C3=runtime identity+epoch exact, C4=lease granted and
        // current, C5=all product resource/access coordinates complete,
        // C6=object protocol valid. R1(all)->construct the existing storage
        // port; R2(any false)->fail before credentials or network are opened.
        let config = parse(manifest(), lease()).unwrap();
        assert_eq!(config.provider, ObjectBackingProvider::Gcs, "R1");
        assert_eq!(config.prefix, "workspaces/a/files");

        for field in ["backing_id", "data_shard_id", "data_authority_epoch"] {
            let mut foreign = lease();
            foreign[field] = if field == "data_authority_epoch" {
                serde_json::json!(4)
            } else {
                serde_json::json!("foreign")
            };
            assert!(parse(manifest(), foreign).is_err(), "R2 C2 {field}");
        }
        let mut wrong_runtime = lease();
        wrong_runtime["runtime"]["cell_incarnation"] = serde_json::json!("foreign");
        assert!(parse(manifest(), wrong_runtime).is_err(), "R2 C3");
        let mut expired = lease();
        expired["expires_at_ms"] = serde_json::json!(200);
        assert!(parse(manifest(), expired).is_err(), "R2 C4");
        let mut revoked = lease();
        revoked["state"] = serde_json::json!("revoked");
        assert!(parse(manifest(), revoked).is_err(), "R2 C4");
        let mut incomplete = manifest();
        incomplete["resources"]["files"]["encryption_key_ref"] = serde_json::json!("");
        assert!(parse(incomplete, lease()).is_err(), "R2 C5");
        let mut protocol_conflict = manifest();
        protocol_conflict["resources"]["files"]["region"] = serde_json::json!("us-central1");
        assert!(parse(protocol_conflict, lease()).is_err(), "R2 C6");
    }

    #[test]
    fn cell_coordinates_are_forbidden_in_the_stable_manifest() {
        // Cause/effect design: adding Cell/runtime/principal data to the stable
        // manifest would couple durable data to an ephemeral failure domain.
        // Closed DTOs reject each extra authority; the lease remains its only owner.
        for field in ["cell_id", "runtime_placement_epoch", "principal_ref"] {
            let mut value = manifest();
            value[field] = serde_json::json!("forbidden");
            assert!(parse(value, lease()).is_err(), "forbidden {field}");
        }
    }

    #[test]
    fn workspace_data_changes_only_blob_bytes_not_relational_metadata_authority() {
        // C1=shared PostgreSQL metadata coordinate, C2=valid manifest+lease.
        // R1(C1+C2)->PostgresObject with the same URL; R2(!C1+C2)->reject
        // instead of silently creating an embedded second source of truth.
        let manifest = write(manifest());
        let lease = write(lease());
        let data = Config {
            manifest_file: manifest.path().into(),
            access_lease_file: lease.path().into(),
            workspace_id: "workspace-a".to_owned().try_into().unwrap(),
            runtime_region_id: "region-a".to_owned().try_into().unwrap(),
            runtime_cell_id: "cell-a".to_owned().try_into().unwrap(),
            runtime_cell_incarnation: "incarnation-a".to_owned().try_into().unwrap(),
            runtime_placement_epoch: 5,
        };
        let backend = resolve(
            Some("postgres://resources/db".into()),
            Some(&data),
            "/data".into(),
        )
        .unwrap();
        assert!(
            matches!(backend, super::super::ResourceStoreBackend::PostgresObject { ref url, .. } if url == "postgres://resources/db")
        );
        assert!(resolve(None, Some(&data), "/data".into()).is_err(), "R2");
    }

    #[tokio::test]
    async fn request_guard_observes_lease_replacement_without_process_restart() {
        // Cause/effect design: C1=the mounted lease is exact and current;
        // C2=the deployment atomically replaces it with Revoked while the
        // process remains alive. R1(C1)->admit; R2(C2)->503 before invoking
        // the business handler. A startup-only snapshot would incorrectly
        // admit R2 after a Cell move.
        let manifest_file = write(manifest());
        let lease_file = write(lease());
        let guard = WorkspaceDataLeaseGuard(Config {
            manifest_file: manifest_file.path().into(),
            access_lease_file: lease_file.path().into(),
            workspace_id: "workspace-a".to_owned().try_into().unwrap(),
            runtime_region_id: "region-a".to_owned().try_into().unwrap(),
            runtime_cell_id: "cell-a".to_owned().try_into().unwrap(),
            runtime_cell_incarnation: "incarnation-a".to_owned().try_into().unwrap(),
            runtime_placement_epoch: 5,
        });
        let app = Router::new()
            .route("/effect", get(|| async { StatusCode::NO_CONTENT }))
            .layer(axum::middleware::from_fn_with_state(
                guard,
                enforce_workspace_data_lease,
            ));
        let request = || {
            HttpRequest::builder()
                .uri("/effect")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::NO_CONTENT,
            "R1"
        );

        let mut revoked = lease();
        revoked["state"] = serde_json::json!("revoked");
        std::fs::write(lease_file.path(), serde_json::to_vec(&revoked).unwrap()).unwrap();
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "R2"
        );
    }
}
