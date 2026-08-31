//! Redacted rendering of the resolved deployment configuration.

use std::collections::BTreeMap;
use std::path::Path;

use awaken_runtime_host::DispatchBackend;

use super::{ResolvedDeployment, Role};

impl ResolvedDeployment {
    pub fn report(&self, json: bool) -> String {
        let owns_control = matches!(self.role, Role::AllInOne | Role::Control);
        let owns_coordinator = matches!(self.role, Role::AllInOne | Role::Coordinator);
        let resource_backend = if !owns_coordinator {
            "not owned by this role"
        } else if self.resources.is_shared() {
            "postgres"
        } else {
            "embedded"
        };
        let runtime_backend = if owns_coordinator {
            match self.runtime.dispatch_backend {
                DispatchBackend::Sqlite => "sqlite",
                DispatchBackend::Postgres => "postgres",
            }
        } else {
            "not owned by this role"
        };
        let control_databases = if owns_control {
            BTreeMap::from([
                ("catalog", render_store_backend(&self.control.catalog)),
                ("credential", render_store_backend(&self.control.credential)),
                ("config", render_store_backend(&self.control.config)),
                ("admin", render_store_backend(&self.control.admin)),
                (
                    "data_subject",
                    render_store_backend(&self.control.data_subject),
                ),
                (
                    "environments",
                    render_store_backend(&self.control.environment),
                ),
            ])
        } else {
            BTreeMap::new()
        };
        let coordinator_databases = if owns_coordinator {
            BTreeMap::from([
                ("sessions", render_store_backend(&self.coordinator.sessions)),
                (
                    "captured_content",
                    render_store_backend(&self.coordinator.captured_content),
                ),
            ])
        } else {
            BTreeMap::new()
        };
        let database_migrations = match self.mode {
            super::OperatingMode::Local => "automatic at startup",
            super::OperatingMode::Server => {
                "explicit `awaken database migrate`; startup verifies only"
            }
        };
        let cloud_credential_source = if self.cloud_iam.service_token_file.is_some() {
            "projected service token"
        } else if self.cloud_iam.developer_key_file.is_some() {
            "workspace Developer Key file plus IAM credential cache"
        } else if self.cloud_iam.access_token.is_some() {
            "explicit access token"
        } else {
            "IAM credential cache"
        };
        if json {
            return serde_json::to_string_pretty(&serde_json::json!({
                "role": self.role.as_str(),
                "mode": self.mode.as_str(),
                "bind": self.bind,
                "internal_bind": self.internal_bind,
                "data_dir": self.data_dir,
                "expected_platform_workspace_id": self.expected_platform_workspace_id,
                "config_file": self.config_path,
                "config_file_exists": self.config_file_exists,
                "no_browser": self.no_browser,
                "ai_sdk_browser_origins": self.ai_sdk_browser_cors.origins(),
                "run_local_pool": self.run_local_pool,
                "identity_mode": identity_mode_name(self.identity_mode),
                "cloud_models": self.cloud_models.as_str(),
                "cloud_identity": {
                    "base_url": self.cloud_iam.base_url,
                    "issuer": self.cloud_iam.issuer,
                    "oauth_client_id": self.cloud_iam.oauth_client_id,
                    "oauth_redirect_uri": self.cloud_iam.oauth_redirect_uri,
                    "credential_source": cloud_credential_source,
                },
                "runtime_dispatch_backend": runtime_backend,
                "resource_backend": resource_backend,
                "control_databases": control_databases,
                "coordinator_databases": coordinator_databases,
                "database_migrations": database_migrations,
                "seal_key": self.seal_key.description(),
                "origins": self.origins,
                "deprecations": self.deprecations,
            }))
            .expect("configuration report is serializable");
        }
        let mut report = format!(
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  internal bind        {internal_bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  AI SDK browser CORS  {browser_cors}\n  identity mode        {identity}\n  cloud models         {cloud_models}\n  Cloud issuer         {cloud_issuer}\n  OAuth client         {oauth_client}\n  OAuth callback       {oauth_callback}\n  credential source    {credential_source}\n  runtime dispatch     {runtime}\n  Resources backend    {resources}\n  control seal key     {key}\n\nSources: command line --config or standard config.toml, then defaults.\n",
            role = self.role.as_str(),
            mode = self.mode.as_str(),
            bind = self.bind,
            internal_bind = self.internal_bind.as_deref().unwrap_or("not applicable"),
            data = self.data_dir.display(),
            config = self.config_path.display(),
            exists = if self.config_file_exists {
                "present"
            } else {
                "not created"
            },
            pool = self.run_local_pool,
            browser_cors = if self.ai_sdk_browser_cors.origins().is_empty() {
                "disabled".to_owned()
            } else {
                self.ai_sdk_browser_cors.origins().join(", ")
            },
            identity = identity_mode_name(self.identity_mode),
            cloud_models = self.cloud_models.as_str(),
            cloud_issuer = self.cloud_iam.issuer,
            oauth_client = self.cloud_iam.oauth_client_id,
            oauth_callback = self.cloud_iam.oauth_redirect_uri,
            credential_source = cloud_credential_source,
            runtime = runtime_backend,
            resources = resource_backend,
            key = self.seal_key.description(),
        );
        if owns_control {
            report.push_str(&format!(
                "\nControl databases (schema: {database_migrations})\n"
            ));
            for (name, backend) in control_databases {
                report.push_str(&format!("  {name:<20} {backend}\n"));
            }
        }
        if owns_coordinator {
            report.push_str(&format!(
                "\nCoordinator databases (schema: {database_migrations})\n"
            ));
            for (name, backend) in coordinator_databases {
                report.push_str(&format!("  {name:<20} {backend}\n"));
            }
        }
        report
    }

    /// Perform startup prerequisites that can be checked without mutating the
    /// deployment. Provider and model validation remains Console-owned because
    /// it requires authenticated product state after startup.
    pub async fn doctor_report(&self, json: bool) -> String {
        let mut data_directory = data_directory_readiness(&self.data_dir);
        let postgres_installation = if data_directory.ready {
            match crate::installation_binding::verify_deployment_installations(self).await {
                Ok(prepared) if prepared.postgres_target_count() == 0 => {
                    Readiness::ready("not applicable".to_owned())
                }
                Ok(prepared) => Readiness::ready(format!(
                    "{} role-owned PostgreSQL target(s) match expected_platform_workspace_id",
                    prepared.postgres_target_count()
                )),
                Err(error) => {
                    data_directory = Readiness::blocked(error.clone());
                    Readiness::blocked(error)
                }
            }
        } else {
            Readiness::blocked("installation admission waits for the data directory".to_owned())
        };
        let listener = match tokio::net::TcpListener::bind(&self.bind).await {
            Ok(listener) => {
                drop(listener);
                Readiness::ready(format!("{} is available", self.bind))
            }
            Err(error) => Readiness::blocked(format!("{}: {error}", self.bind)),
        };
        let ready = data_directory.ready && postgres_installation.ready && listener.ready;
        if json {
            return serde_json::to_string_pretty(&serde_json::json!({
                "ready": ready,
                "configuration": {
                    "status": "ready",
                    "path": self.config_path,
                    "exists": self.config_file_exists,
                },
                "data_directory": data_directory.as_json(),
                "postgres_installation": postgres_installation.as_json(),
                "listener": listener.as_json(),
                "next": if ready {
                    "start Awaken, then validate a model or provider in Console"
                } else {
                    "resolve blocked checks, then run awaken doctor again"
                },
            }))
            .expect("doctor report is serializable");
        }

        format!(
            "Awaken doctor\n\n  configuration       ready ({config})\n  data directory      {data_status} ({data_detail})\n  PostgreSQL identity {postgres_status} ({postgres_detail})\n  listener            {listener_status} ({listener_detail})\n\nResult: {result}\nNext: {next}\n",
            config = self.config_path.display(),
            data_status = data_directory.status,
            data_detail = data_directory.detail,
            postgres_status = postgres_installation.status,
            postgres_detail = postgres_installation.detail,
            listener_status = listener.status,
            listener_detail = listener.detail,
            result = if ready { "ready to start" } else { "blocked" },
            next = if ready {
                "start Awaken, then validate a model or provider in Console"
            } else {
                "resolve blocked checks, then run `awaken doctor` again"
            },
        )
    }
}

struct Readiness {
    ready: bool,
    status: &'static str,
    detail: String,
}

impl Readiness {
    fn ready(detail: String) -> Self {
        Self {
            ready: true,
            status: "ready",
            detail,
        }
    }

    fn ready_to_create(detail: String) -> Self {
        Self {
            ready: true,
            status: "ready_to_create",
            detail,
        }
    }

    fn blocked(detail: String) -> Self {
        Self {
            ready: false,
            status: "blocked",
            detail,
        }
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "detail": self.detail,
        })
    }
}

fn data_directory_readiness(path: &Path) -> Readiness {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Readiness::ready(format!("{} exists", path.display())),
        Ok(_) => Readiness::blocked(format!("{} is not a directory", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let existing_parent = path.ancestors().skip(1).find(|parent| parent.exists());
            match existing_parent.and_then(|parent| std::fs::metadata(parent).ok()) {
                Some(metadata) if metadata.is_dir() => Readiness::ready_to_create(format!(
                    "{} will be created under an existing directory",
                    path.display()
                )),
                _ => {
                    Readiness::blocked(format!("{} has no usable parent directory", path.display()))
                }
            }
        }
        Err(error) => Readiness::blocked(format!("{}: {error}", path.display())),
    }
}

const fn identity_mode_name(mode: awaken_control::ManagementIdentityMode) -> &'static str {
    match mode {
        awaken_control::ManagementIdentityMode::NoLogin => "no-login",
        awaken_control::ManagementIdentityMode::AwakenCloud => "awaken-cloud",
        awaken_control::ManagementIdentityMode::SelfManaged => "self-managed",
    }
}

fn render_store_backend(backend: &awaken_control::StoreBackend) -> String {
    match backend {
        awaken_control::StoreBackend::Sqlite(path) => {
            format!("sqlite ({})", path.display())
        }
        awaken_control::StoreBackend::Postgres(_) => "postgres (URL redacted)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn doctor_covers_storage_and_listener_readiness_without_creating_state() {
        // Cause/effect graph: exact local installation + data-path metadata +
        // bind result -> independent readiness details -> one overall result.
        // Constraints: diagnostics never initialize an absent directory and do
        // not retain the test-bound listener. Decision table: R1 bound storage +
        // free listener=>ready; R2 absent/unbound child=>blocked and zero writes;
        // R3 non-directory path=>blocked; R4 bound storage+occupied listener=>
        // listener blocked.
        let directory = tempfile::tempdir().unwrap();
        drop(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &directory.path().join("sessions.db").to_string_lossy(),
            )
            .unwrap(),
        );
        std::fs::write(
            directory.path().join("platform-workspace-id"),
            "workspace-local",
        )
        .unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.bind = "127.0.0.1:0".to_owned();

        let ready: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(ready["ready"], true, "R1");
        assert_eq!(ready["data_directory"]["status"], "ready", "R1");

        let absent = directory.path().join("new").join("data");
        deployment.data_dir = absent.clone();
        deployment.coordinator.sessions =
            awaken_control::StoreBackend::Sqlite(absent.join("sessions.db"));
        let creatable: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(creatable["ready"], false, "R2");
        assert_eq!(creatable["data_directory"]["status"], "blocked", "R2");
        assert!(
            creatable["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("unbound_empty_local_storage:"),
            "R2: {creatable}"
        );
        assert!(!absent.exists(), "doctor remains read-only");

        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, b"fixture").unwrap();
        deployment.data_dir = file.clone();
        deployment.coordinator.sessions =
            awaken_control::StoreBackend::Sqlite(file.join("sessions.db"));
        let blocked_storage: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(blocked_storage["ready"], false, "R3");
        assert_eq!(blocked_storage["data_directory"]["status"], "blocked", "R3");

        deployment.data_dir = directory.path().to_path_buf();
        deployment.coordinator.sessions =
            awaken_control::StoreBackend::Sqlite(directory.path().join("sessions.db"));
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        deployment.bind = occupied.local_addr().unwrap().to_string();
        let blocked_listener: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(blocked_listener["ready"], false, "R4");
        assert_eq!(blocked_listener["listener"]["status"], "blocked", "R4");
    }

    #[tokio::test]
    async fn doctor_is_exact_only_for_marker_first_and_expected_identity() {
        /* Cause/effect graph: C1 expected Workspace absent/present; C2 marker
         * absent/matching/mismatched; C3 Session absent. Effects: E1 Doctor is
         * always read-only and blocks incomplete initialization; E2 configured
         * identity adds an exact marker fence. Decision table: SD1 marker-first+
         * no expected=>session_storage_missing; SD2 exact marker+Session is ready
         * (neighboring test); SD3 expected+matching marker+missing Session=>same
         * missing error; SD4 expected+missing marker=>platform_workspace_missing;
         * SD5 expected+mismatch=>mismatch. */
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("platform-workspace-id"),
            "workspace_local_regression",
        )
        .unwrap();
        let sessions = directory.path().join("sessions.db");
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.bind = "127.0.0.1:0".to_owned();

        let role_first: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(role_first["ready"], false, "SD1");
        assert!(
            role_first["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("session_storage_missing:"),
            "SD1: {role_first}"
        );
        assert!(!sessions.exists(), "SD1 doctor remains read-only");

        deployment.expected_platform_workspace_id = Some("workspace_local_regression".into());
        let report: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(report["ready"], false, "SD3");
        assert_eq!(report["data_directory"]["status"], "blocked", "SD3");
        assert!(
            report["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("session_storage_missing:"),
            "SD3 stable diagnostic: {report}"
        );
        assert!(!sessions.exists(), "SD3/E3 doctor remains read-only");

        let empty = tempfile::tempdir().unwrap();
        deployment.data_dir = empty.path().to_path_buf();
        deployment.coordinator.sessions =
            awaken_control::StoreBackend::Sqlite(empty.path().join("sessions.db"));
        let missing_identity: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert!(
            missing_identity["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("platform_workspace_missing:"),
            "SD4: {missing_identity}"
        );
        assert_eq!(
            std::fs::read_dir(empty.path()).unwrap().count(),
            0,
            "SD4/E3"
        );

        std::fs::write(
            empty.path().join("platform-workspace-id"),
            "workspace-other",
        )
        .unwrap();
        let mismatch: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert!(
            mismatch["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("platform_workspace_mismatch:"),
            "SD5: {mismatch}"
        );
    }

    #[tokio::test]
    async fn doctor_blocks_store_first_recovery_and_accepts_normal_restart_read_only() {
        /* Decision-table rules SD2a/SD2b: canonical Session without a marker is
         * a store-first crash window and Doctor blocks until an explicitly
         * authorized migration completes it; the same Session with its marker
         * is a normal restart. Both observations preserve every byte. */
        fn snapshot(directory: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
            std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect()
        }

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("platform-workspace-id");
        let sessions = directory.path().join("sessions.db");
        drop(
            awaken_session_store::SqliteManagedSessionRepository::open(&sessions.to_string_lossy())
                .unwrap(),
        );
        let before = snapshot(directory.path());
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.bind = "127.0.0.1:0".to_owned();

        let report: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(report["ready"], false, "SD2a");
        assert_eq!(report["data_directory"]["status"], "blocked", "SD2a");
        assert!(
            report["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("local_installation_incomplete:"),
            "SD2a: {report}"
        );
        assert_eq!(snapshot(directory.path()), before, "SD2a read-only");
        assert!(!marker.exists(), "SD2a doctor does not publish identity");

        std::fs::write(&marker, "workspace_local_regression").unwrap();
        deployment.expected_platform_workspace_id = Some("workspace_local_regression".into());
        let before = snapshot(directory.path());
        let report: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(report["ready"], true, "SD2b");
        assert_eq!(report["data_directory"]["status"], "ready", "SD2b");
        assert_eq!(snapshot(directory.path()), before, "SD2b read-only");
    }

    #[tokio::test]
    async fn doctor_rejects_initialized_storage_with_truncated_session_authority() {
        /* Decision-table rule SD4 in the preceding test has two physical
         * corruption classes. A zero-byte authority has a stable local reason;
         * a non-empty malformed file delegates diagnosis to the canonical
         * Session SQLite probe. Both remain byte-for-byte unchanged. */
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("platform-workspace-id"),
            "workspace_local_regression",
        )
        .unwrap();
        let sessions = directory.path().join("sessions.db");
        std::fs::write(&sessions, []).unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.bind = "127.0.0.1:0".to_owned();

        let report: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(report["ready"], false, "SD4 zero-byte");
        assert_eq!(
            report["data_directory"]["status"], "blocked",
            "SD4 zero-byte"
        );
        assert!(
            report["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("session_storage_empty:"),
            "SD4 stable zero-byte diagnostic: {report}"
        );
        assert_eq!(std::fs::metadata(&sessions).unwrap().len(), 0, "SD4/E3");

        let truncated = b"SQLite format 3\0truncated";
        std::fs::write(&sessions, truncated).unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(report["ready"], false, "SD4 malformed");
        assert_eq!(
            report["data_directory"]["status"], "blocked",
            "SD4 malformed"
        );
        assert!(
            report["data_directory"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("session_storage_invalid:"),
            "SD4 stable malformed diagnostic: {report}"
        );
        assert_eq!(std::fs::read(sessions).unwrap(), truncated, "SD4/E3");
    }
}
