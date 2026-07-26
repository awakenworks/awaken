//! Per-component database selection for the control (config) plane.
//!
//! The management plane owns several independent stores — the model catalog, the
//! credential vault (+ sealed secrets), the config authoring registry, the admin
//! aggregate (inference profiles / MCP defs / webhook subscriptions), and the
//! managed-session repository. Historically they were all SQLite files bundled under
//! one `AWAKEN_MGMT_DIR`. This module lets each one be pointed at its **own** database
//! independently, so an operator can isolate (e.g.) credentials on a hardened Postgres
//! while config stays on another — the precondition for splitting control / server
//! into separate services that share per-component databases (ADR: shared-DB, Option A).
//!
//! Each component reads an `AWAKEN_<COMPONENT>_DB` override:
//!   - a `postgres://` / `postgresql://` URL  → the Postgres backend,
//!   - any other value                        → a SQLite file at that path,
//!   - unset                                  → `<AWAKEN_MGMT_DIR>/<name>.db` (SQLite).
//!
//! The bundle default preserves today's behavior exactly when no override is set.

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

/// The typed database configuration backing each control-plane store.
#[derive(Debug, Clone)]
pub struct ControlStoreConfig {
    pub catalog: StoreBackend,
    /// The credential repo AND its sealed-secret blobs share this one backend (they
    /// are the same `credential.db` today; the same Postgres database when shared).
    pub credential: StoreBackend,
    pub config: StoreBackend,
    /// The admin aggregate: inference profiles, MCP server defs, webhook subscriptions.
    pub admin: StoreBackend,
    pub sessions: StoreBackend,
}

impl ControlStoreConfig {
    #[must_use]
    pub fn local(dir: &Path) -> Self {
        Self::from_values(dir, None, None, None, None, None)
    }

    #[must_use]
    pub fn from_values(
        dir: &Path,
        catalog: Option<String>,
        credential: Option<String>,
        config: Option<String>,
        admin: Option<String>,
        sessions: Option<String>,
    ) -> Self {
        let bundle = |name: &str| dir.join(name);
        Self {
            catalog: StoreBackend::resolve(catalog, bundle("catalog.db")),
            credential: StoreBackend::resolve(credential, bundle("credential.db")),
            config: StoreBackend::resolve(config, bundle("config.db")),
            admin: StoreBackend::resolve(admin, bundle("admin.db")),
            sessions: StoreBackend::resolve(sessions, bundle("sessions.db")),
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
            get("AWAKEN_SESSIONS_DB"),
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
            c.sessions,
            StoreBackend::Sqlite("/var/awaken/sessions.db".into())
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
    fn each_component_resolves_independently() {
        let c = cfg(&[
            ("AWAKEN_CATALOG_DB", "postgres://h/cat"),
            ("AWAKEN_CREDENTIAL_DB", "postgres://secure/cred"),
            ("AWAKEN_SESSIONS_DB", "/data/sessions.db"),
        ]);
        assert_eq!(c.catalog, StoreBackend::Postgres("postgres://h/cat".into()));
        assert_eq!(
            c.credential,
            StoreBackend::Postgres("postgres://secure/cred".into())
        );
        assert_eq!(c.sessions, StoreBackend::Sqlite("/data/sessions.db".into()));
        assert_eq!(c.admin, StoreBackend::Sqlite("/var/awaken/admin.db".into()));
    }
}
