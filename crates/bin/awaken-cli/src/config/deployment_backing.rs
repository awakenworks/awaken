//! Secret-free deployment backing contract projected by the hosting platform.

use std::path::Path;

use awaken_resource_persistence::{ObjectBackingConfig, ObjectBackingProvider};
use serde::Deserialize;

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

pub(super) fn load(path: &Path) -> Result<ObjectBackingConfig, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read deployment_backing_file {}: {error}", path.display()))?;
    let contract: Contract = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse deployment_backing_file {}: {error}", path.display()))?;
    if contract.schema_version != 2 {
        return Err("deployment_backing_file schema_version must be 2".into());
    }
    for (name, value) in [
        ("binding_id", contract.binding_id.as_str()),
        ("consumer_ref", contract.consumer_ref.as_str()),
        ("scope_ref", contract.scope_ref.as_str()),
        ("cell_id", contract.cell_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("deployment_backing_file {name} must be non-empty"));
        }
    }
    if contract.requested_generation == 0 {
        return Err("deployment_backing_file requested_generation must be positive".into());
    }
    let mut database_roles = std::collections::BTreeSet::new();
    for database in &contract.databases {
        if !database_roles.insert(database.role.as_str()) {
            return Err("deployment_backing_file contains duplicate database roles".into());
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
                    "deployment_backing_file database.{name} must be non-empty"
                ));
            }
        }
    }
    let mut secret_roles = std::collections::BTreeSet::new();
    for secret in &contract.secrets {
        if !secret_roles.insert(secret.role.as_str()) {
            return Err("deployment_backing_file contains duplicate secret roles".into());
        }
        for (name, value) in [
            ("role", secret.role.as_str()),
            ("secret_ref", secret.secret_ref.as_str()),
            ("key_ref", secret.key_ref.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "deployment_backing_file secret.{name} must be non-empty"
                ));
            }
        }
    }
    let mut files = contract
        .objects
        .into_iter()
        .filter(|allocation| allocation.role == "files");
    let allocation = files.next().ok_or_else(|| {
        "deployment_backing_file requires exactly one files object role".to_owned()
    })?;
    if files.next().is_some() {
        return Err("deployment_backing_file contains duplicate files object roles".into());
    }
    for (name, value) in [
        ("bucket_ref", allocation.bucket_ref.as_str()),
        ("prefix_ref", allocation.prefix_ref.as_str()),
        ("identity_ref", allocation.identity_ref.as_str()),
        ("encryption_key_ref", allocation.encryption_key_ref.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!(
                "deployment_backing_file files.{name} must be non-empty"
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
    backing_file: Option<&Path>,
    embedded_root: std::path::PathBuf,
) -> Result<super::ResourceStoreBackend, String> {
    let object = backing_file.map(load).transpose()?;
    match (database_url, object) {
        (Some(url), Some(object)) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::PostgresObject { url, object })
        }
        (Some(url), None) if super::file_support::is_postgres_url(&url) => {
            Ok(super::ResourceStoreBackend::Postgres(url))
        }
        (None, Some(_)) => {
            Err("deployment_backing_file requires shared PostgreSQL Resources metadata".into())
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

    #[test]
    fn contract_requires_one_exact_secret_free_files_allocation() {
        // Cause/effect graph: C1=schema/identity/generation valid, C2=unique
        // complete database evidence, C3=exactly one files role, C4=all
        // database/object/secret custody refs non-empty and secret roles unique,
        // C5=protocol coordinates compatible. R1(C1..C5)->one ObjectFileStore
        // config; R2(any false)->
        // fail before database, credentials, or object network are opened.
        let file = write(valid());
        let config = load(file.path()).unwrap();
        assert_eq!(config.provider, ObjectBackingProvider::Gcs);
        assert_eq!(config.prefix, "deployments/a/files");

        let mut duplicate = valid();
        let duplicate_role = duplicate["objects"][0].clone();
        duplicate["objects"]
            .as_array_mut()
            .unwrap()
            .push(duplicate_role);
        assert!(load(write(duplicate).path()).is_err());
        let mut empty_identity = valid();
        empty_identity["objects"][0]["identity_ref"] = serde_json::json!("");
        assert!(load(write(empty_identity).path()).is_err());
        let mut empty_database = valid();
        empty_database["databases"][0]["principal_ref"] = serde_json::json!("");
        assert!(load(write(empty_database).path()).is_err());
        let mut empty_connection_ref = valid();
        empty_connection_ref["databases"][0]["connection_secret_ref"] = serde_json::json!("");
        assert!(load(write(empty_connection_ref).path()).is_err());
        let mut provider_conflict = valid();
        provider_conflict["objects"][0]["region"] = serde_json::json!("us-central1");
        assert!(load(write(provider_conflict).path()).is_err());
    }

    #[test]
    fn backing_changes_only_blob_bytes_not_relational_metadata_authority() {
        // Cause/effect graph: C1=shared PostgreSQL coordinate, C2=valid exact
        // backing artifact. R1(C1+C2)->PostgresObject with the same metadata
        // URL; R2(!C1+C2)->reject rather than silently opening embedded state.
        let file = write(valid());
        let backend = resolve(
            Some("postgres://resources/db".into()),
            Some(file.path()),
            "/data".into(),
        )
        .unwrap();
        assert!(matches!(
            backend,
            super::super::ResourceStoreBackend::PostgresObject { ref url, ref object }
                if url == "postgres://resources/db" && object.prefix == "deployments/a/files"
        ));
        assert!(resolve(None, Some(file.path()), "/data".into()).is_err());
    }
}
