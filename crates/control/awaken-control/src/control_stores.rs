//! Database-address bundle for the Control bounded context.
//!
//! Each component receives one typed deployment store value:
//!   - a `postgres://` / `postgresql://` URL  → the Postgres backend,
//!   - any other value                        → a SQLite file at that path,
//!   - absent                                 → `<data_dir>/<name>.db` (SQLite).
//!
//! AllInOne resolves the complete bundle to local SQLite by default. Split roles
//! reject explicitly configured foreign-domain fields before store acquisition.

use std::path::{Path, PathBuf};

/// Where one control-plane store lives: a local SQLite file, or a shared Postgres.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreBackend {
    Sqlite(PathBuf),
    Postgres(String),
}

impl StoreBackend {
    /// Resolve an override value against a bundle default. A `postgres(ql)://` value is
    /// a Postgres URL; any other value is a SQLite path; `None` falls back to the
    /// bundle file (`<dir>/<name>`).
    pub fn resolve(override_value: Option<String>, default_path: PathBuf) -> Self {
        match override_value {
            Some(value) if is_postgres_url(&value) => StoreBackend::Postgres(value),
            Some(value) => StoreBackend::Sqlite(PathBuf::from(value)),
            None => StoreBackend::Sqlite(default_path),
        }
    }
}

fn is_postgres_url(value: &str) -> bool {
    value.starts_with("postgres://") || value.starts_with("postgresql://")
}

/// Typed database addresses for the process composition compatibility bundle.
/// Role-aware resolution and acquisition decide which fields a process may use.
#[derive(Debug, Clone)]
pub struct ControlStoreConfig {
    pub catalog: StoreBackend,
    /// The credential repo AND its sealed-secret blobs share this one backend (they
    /// are the same `credential.db` today; the same Postgres database when shared).
    pub credential: StoreBackend,
    pub config: StoreBackend,
    /// The admin aggregate: inference profiles, Agent inputs, and webhooks.
    pub admin: StoreBackend,
    /// The Control-owned Data Subject aggregate and erasure process checkpoints.
    pub data_subject: StoreBackend,
    /// Control-owned Environment definitions, revision history, and policies.
    pub environment: StoreBackend,
}

impl ControlStoreConfig {
    #[must_use]
    pub fn local(dir: &Path) -> Self {
        Self::from_values(dir, None, None, None, None, None, None)
    }

    #[must_use]
    pub fn from_values(
        dir: &Path,
        catalog: Option<String>,
        credential: Option<String>,
        config: Option<String>,
        admin: Option<String>,
        data_subject: Option<String>,
        environment: Option<String>,
    ) -> Self {
        let bundle = |name: &str| dir.join(name);
        Self {
            catalog: StoreBackend::resolve(catalog, bundle("catalog.db")),
            credential: StoreBackend::resolve(credential, bundle("credential.db")),
            config: StoreBackend::resolve(config, bundle("config.db")),
            admin: StoreBackend::resolve(admin, bundle("admin.db")),
            data_subject: StoreBackend::resolve(data_subject, bundle("data_subject.db")),
            environment: StoreBackend::resolve(environment, bundle("environments.db")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(env: &[(&str, &str)]) -> ControlStoreConfig {
        let dir = Path::new("/var/awaken");
        let get = |key| {
            env.iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| value.to_string())
        };
        ControlStoreConfig::from_values(
            dir,
            get("AWAKEN_CATALOG_DB"),
            get("AWAKEN_CREDENTIAL_DB"),
            get("AWAKEN_CONFIG_DB"),
            get("AWAKEN_ADMIN_DB"),
            get("AWAKEN_DATA_SUBJECT_DB"),
            get("AWAKEN_ENVIRONMENT_DB"),
        )
    }

    #[test]
    fn unset_falls_back_to_the_bundle_sqlite_files() {
        let c = cfg(&[]);
        assert_eq!(
            c.catalog,
            StoreBackend::Sqlite("/var/awaken/catalog.db".into())
        );
        assert_eq!(
            c.credential,
            StoreBackend::Sqlite("/var/awaken/credential.db".into())
        );
        assert_eq!(
            c.config,
            StoreBackend::Sqlite("/var/awaken/config.db".into())
        );
        assert_eq!(c.admin, StoreBackend::Sqlite("/var/awaken/admin.db".into()));
        assert_eq!(
            c.data_subject,
            StoreBackend::Sqlite("/var/awaken/data_subject.db".into())
        );
        assert_eq!(
            c.environment,
            StoreBackend::Sqlite("/var/awaken/environments.db".into())
        );
    }

    #[test]
    fn a_postgres_url_selects_the_postgres_backend() {
        let c = cfg(&[("AWAKEN_CREDENTIAL_DB", "postgres://h/creds")]);
        assert_eq!(
            c.credential,
            StoreBackend::Postgres("postgres://h/creds".into())
        );
        // Other components are untouched — each is independent.
        assert_eq!(
            c.config,
            StoreBackend::Sqlite("/var/awaken/config.db".into())
        );
    }

    #[test]
    fn a_non_url_override_is_a_sqlite_path_elsewhere() {
        let c = cfg(&[
            ("AWAKEN_CATALOG_DB", "/mnt/fast/catalog.db"),
            ("AWAKEN_CONFIG_DB", "postgresql://h/cfg"),
        ]);
        assert_eq!(
            c.catalog,
            StoreBackend::Sqlite("/mnt/fast/catalog.db".into())
        );
        assert_eq!(
            c.config,
            StoreBackend::Postgres("postgresql://h/cfg".into())
        );
    }

    #[test]
    fn each_control_component_resolves_independently() {
        // Causes: explicit Catalog and Credential bindings select different
        // backends. Effects: each Control store keeps its exact backend and an
        // unconfigured Admin store retains its local default.
        let c = cfg(&[
            ("AWAKEN_CATALOG_DB", "postgres://h/cat"),
            ("AWAKEN_CREDENTIAL_DB", "postgres://secure/cred"),
        ]);
        assert_eq!(c.catalog, StoreBackend::Postgres("postgres://h/cat".into()));
        assert_eq!(
            c.credential,
            StoreBackend::Postgres("postgres://secure/cred".into())
        );
        assert_eq!(c.admin, StoreBackend::Sqlite("/var/awaken/admin.db".into()));
    }
}
