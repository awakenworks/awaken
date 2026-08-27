//! Secret-free deployment backing contract projected by the hosting platform.

use std::path::PathBuf;

use awaken_resource_contract::{
    DeploymentBackingAllocationKind, DeploymentBackingRole, select_deployment_backing_role,
};
use awaken_resource_persistence::{ObjectBackingConfig, ObjectBackingProvider};
use serde::Deserialize;

/// Deployment-owned expectation for Cloud's immutable backing artifact.
/// Every authority coordinate is compared before any store is opened.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    file: PathBuf,
    binding_id: String,
    consumer_ref: String,
    scope_ref: String,
    cell_id: String,
    requested_generation: u64,
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.file.as_os_str().is_empty() {
            return Err("deployment_backing.file must not be empty".into());
        }
        for (name, value) in [
            ("binding_id", self.binding_id.as_str()),
            ("consumer_ref", self.consumer_ref.as_str()),
            ("scope_ref", self.scope_ref.as_str()),
            ("cell_id", self.cell_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("deployment_backing.{name} must be non-empty"));
            }
        }
        if self.requested_generation == 0 {
            return Err("deployment_backing.requested_generation must be positive".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    schema_version: u32,
    binding_id: String,
    consumer_ref: String,
    scope_ref: String,
    cell_id: String,
    requested_generation: u64,
    databases: Vec<DatabaseAllocation>,
    objects: Vec<ObjectAllocation>,
    secrets: Vec<SecretAllocation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseAllocation {
    role: String,
    database_ref: String,
    schema_ref: String,
    principal_ref: String,
    connection_secret_ref: String,
    connection_secret_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretAllocation {
    role: String,
    secret_ref: String,
    key_ref: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectAllocation {
    role: String,
    protocol: Protocol,
    bucket_ref: String,
    prefix_ref: String,
    identity_ref: String,
    encryption_key_ref: String,
    region: Option<String>,
    endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Protocol {
    S3,
    Gcs,
}

pub(super) fn load(expected: &Config) -> Result<ObjectBackingConfig, String> {
    expected.validate()?;
    let bytes = std::fs::read(&expected.file).map_err(|error| {
        format!(
            "read deployment_backing.file {}: {error}",
            expected.file.display()
        )
    })?;
    let contract: Contract = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse deployment_backing.file {}: {error}",
            expected.file.display()
        )
    })?;
    if contract.schema_version != 2 {
        return Err("deployment backing artifact schema_version must be 2".into());
    }
    for (name, value) in [
        ("binding_id", contract.binding_id.as_str()),
        ("consumer_ref", contract.consumer_ref.as_str()),
        ("scope_ref", contract.scope_ref.as_str()),
        ("cell_id", contract.cell_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!(
                "deployment backing artifact {name} must be non-empty"
            ));
        }
    }
    if contract.requested_generation == 0 {
        return Err("deployment backing artifact requested_generation must be positive".into());
    }
    if contract.binding_id != expected.binding_id
        || contract.consumer_ref != expected.consumer_ref
        || contract.scope_ref != expected.scope_ref
        || contract.cell_id != expected.cell_id
        || contract.requested_generation != expected.requested_generation
    {
        return Err(
            "deployment backing artifact does not match the configured deployment coordinates"
                .into(),
        );
    }
    let mut database_roles = std::collections::BTreeSet::new();
    for database in &contract.databases {
        if !database_roles.insert(database.role.as_str()) {
            return Err("deployment backing artifact contains duplicate database roles".into());
        }
        for (name, value) in [
            ("role", database.role.as_str()),
            ("database_ref", database.database_ref.as_str()),
            ("schema_ref", database.schema_ref.as_str()),
            ("principal_ref", database.principal_ref.as_str()),
            (
                "connection_secret_ref",
                database.connection_secret_ref.as_str(),
            ),
            (
                "connection_secret_key",
                database.connection_secret_key.as_str(),
            ),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "deployment backing artifact database.{name} must be non-empty"
                ));
            }
        }
    }
    let mut resource_databases = contract.databases.iter().filter(|database| {
        select_deployment_backing_role(
            DeploymentBackingAllocationKind::Database,
            database.role.as_bytes(),
        ) == Some(DeploymentBackingRole::ResourcesDatabase)
    });
    resource_databases.next().ok_or_else(|| {
        "deployment backing artifact requires exactly one resources database role".to_owned()
    })?;
    if resource_databases.next().is_some() {
        return Err(
            "deployment backing artifact contains duplicate resources database roles".into(),
        );
    }
    let mut secret_roles = std::collections::BTreeSet::new();
    for secret in &contract.secrets {
        if !secret_roles.insert(secret.role.as_str()) {
            return Err("deployment backing artifact contains duplicate secret roles".into());
        }
        for (name, value) in [
            ("role", secret.role.as_str()),
            ("secret_ref", secret.secret_ref.as_str()),
            ("key_ref", secret.key_ref.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "deployment backing artifact secret.{name} must be non-empty"
                ));
            }
        }
    }
    let mut files = contract.objects.into_iter().filter(|allocation| {
        select_deployment_backing_role(
            DeploymentBackingAllocationKind::Object,
            allocation.role.as_bytes(),
        ) == Some(DeploymentBackingRole::FilesObject)
    });
    let allocation = files.next().ok_or_else(|| {
        "deployment backing artifact requires exactly one files object role".to_owned()
    })?;
    if files.next().is_some() {
        return Err("deployment backing artifact contains duplicate files object roles".into());
    }
    for (name, value) in [
        ("bucket_ref", allocation.bucket_ref.as_str()),
        ("prefix_ref", allocation.prefix_ref.as_str()),
        ("identity_ref", allocation.identity_ref.as_str()),
        ("encryption_key_ref", allocation.encryption_key_ref.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!(
                "deployment backing artifact files.{name} must be non-empty"
            ));
        }
    }
    let config = ObjectBackingConfig {
        provider: match allocation.protocol {
            Protocol::S3 => ObjectBackingProvider::S3,
            Protocol::Gcs => ObjectBackingProvider::Gcs,
        },
        bucket: allocation.bucket_ref,
        prefix: allocation.prefix_ref,
        region: allocation.region,
        endpoint: allocation.endpoint,
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

pub(super) fn resolve(
    database_url: Option<String>,
    backing: Option<&Config>,
    embedded_root: std::path::PathBuf,
) -> Result<super::ResourceStoreBackend, String> {
    let object = backing.map(load).transpose()?;
    match (database_url, object) {
        (Some(url), Some(object)) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::PostgresObject { url, object })
        }
        (Some(url), None) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::Postgres(url))
        }
        (None, Some(_)) => {
            Err("deployment_backing requires shared PostgreSQL Resources metadata".into())
        }
        (Some(_), _) => Err("resource_database_url must be postgres://".into()),
        (None, None) => Ok(super::ResourceStoreBackend::Embedded(embedded_root)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(value: serde_json::Value) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        file
    }

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2,
            "binding_id": "binding-a",
            "consumer_ref": "deployment/a",
            "scope_ref": "workspace/a",
            "cell_id": "cell-a",
            "requested_generation": 3,
            "databases": [{
                "role": "resources", "database_ref": "databases/awaken-a",
                "schema_ref": "schemas/resources", "principal_ref": "identities/resources",
                "connection_secret_ref": "awaken-a-resources-database",
                "connection_secret_key": "url"
            }],
            "objects": [{
                "role": "files", "protocol": "gcs", "bucket_ref": "awaken-a",
                "prefix_ref": "deployments/a/files", "identity_ref": "identities/files",
                "encryption_key_ref": "keys/a", "region": null, "endpoint": null
            }],
            "secrets": []
        })
    }

    fn parse(value: serde_json::Value) -> Result<ObjectBackingConfig, String> {
        let file = write(value);
        load(&Config {
            file: file.path().to_path_buf(),
            binding_id: "binding-a".into(),
            consumer_ref: "deployment/a".into(),
            scope_ref: "workspace/a".into(),
            cell_id: "cell-a".into(),
            requested_generation: 3,
        })
    }

    #[test]
    fn config_rejects_legacy_or_extra_authority_inputs() {
        // Cause/effect design: a path-only legacy field or an extra authority
        // coordinate creates two possible deployment facts. The closed typed
        // table admits exactly file + binding + consumer + scope + Cell + generation.
        let legacy = r#"deployment_backing_file = "/legacy/backing.json""#;
        let error = toml::from_str::<super::super::file_schema::FileConfig>(legacy).unwrap_err();
        assert!(error.to_string().contains("unknown field"));

        let extra = r#"
file = "/etc/awaken/deployment-backing.json"
binding_id = "binding-a"
consumer_ref = "deployment/a"
scope_ref = "workspace/a"
cell_id = "cell-a"
requested_generation = 3
foreign_authority = "forbidden"
"#;
        let error = toml::from_str::<Config>(extra).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn contract_requires_one_exact_secret_free_files_allocation() {
        // Cause/effect graph: C1=schema/identity/generation valid, C2=unique
        // complete database evidence, C3=exactly one files role, C4=all
        // database/object/secret custody refs non-empty and secret roles unique,
        // C5=protocol coordinates compatible. R1(C1..C5)->one ObjectFileStore
        // config; R2(any false)->
        // fail before database, credentials, or object network are opened.
        let config = parse(valid()).unwrap();
        assert_eq!(config.provider, ObjectBackingProvider::Gcs);
        assert_eq!(config.prefix, "deployments/a/files");

        let mut duplicate = valid();
        let duplicate_role = duplicate["objects"][0].clone();
        duplicate["objects"]
            .as_array_mut()
            .unwrap()
            .push(duplicate_role);
        assert!(parse(duplicate).is_err());
        let mut empty_identity = valid();
        empty_identity["objects"][0]["identity_ref"] = serde_json::json!("");
        assert!(parse(empty_identity).is_err());
        let mut empty_database = valid();
        empty_database["databases"][0]["principal_ref"] = serde_json::json!("");
        assert!(parse(empty_database).is_err());
        let mut empty_connection_ref = valid();
        empty_connection_ref["databases"][0]["connection_secret_ref"] = serde_json::json!("");
        assert!(parse(empty_connection_ref).is_err());
        let mut missing_resources_database = valid();
        missing_resources_database["databases"] = serde_json::json!([]);
        assert!(parse(missing_resources_database).is_err());
        let mut unrelated_database = valid();
        unrelated_database["databases"][0]["role"] = serde_json::json!("analytics");
        assert!(parse(unrelated_database).is_err());
        let mut provider_conflict = valid();
        provider_conflict["objects"][0]["region"] = serde_json::json!("us-central1");
        assert!(parse(provider_conflict).is_err());

        // Incremental MC/DC design for authority coordinates: keep allocation
        // evidence valid and mutate one artifact coordinate per row. Every row
        // must fail before database credentials or object storage are opened.
        for field in [
            "binding_id",
            "consumer_ref",
            "scope_ref",
            "cell_id",
            "requested_generation",
        ] {
            let mut foreign = valid();
            foreign[field] = if field == "requested_generation" {
                serde_json::json!(4)
            } else {
                serde_json::json!("foreign")
            };
            assert!(parse(foreign).is_err(), "coordinate {field}");
        }
    }

    #[test]
    fn backing_changes_only_blob_bytes_not_relational_metadata_authority() {
        // Cause/effect graph: C1=shared PostgreSQL coordinate, C2=valid exact
        // backing artifact. R1(C1+C2)->PostgresObject with the same metadata
        // URL; R2(!C1+C2)->reject rather than silently opening embedded state.
        let file = write(valid());
        let backing = Config {
            file: file.path().to_path_buf(),
            binding_id: "binding-a".into(),
            consumer_ref: "deployment/a".into(),
            scope_ref: "workspace/a".into(),
            cell_id: "cell-a".into(),
            requested_generation: 3,
        };
        let backend = resolve(
            Some("postgres://resources/db".into()),
            Some(&backing),
            "/data".into(),
        )
        .unwrap();
        assert!(matches!(
            backend,
            super::super::ResourceStoreBackend::PostgresObject { ref url, ref object }
                if url == "postgres://resources/db" && object.prefix == "deployments/a/files"
        ));
        assert!(resolve(None, Some(&backing), "/data".into()).is_err());
    }
}
