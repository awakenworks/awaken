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
        let data_directory = data_directory_readiness(&self.data_dir);
        let listener = match tokio::net::TcpListener::bind(&self.bind).await {
            Ok(listener) => {
                drop(listener);
                Readiness::ready(format!("{} is available", self.bind))
            }
            Err(error) => Readiness::blocked(format!("{}: {error}", self.bind)),
        };
        let ready = data_directory.ready && listener.ready;
        if json {
            return serde_json::to_string_pretty(&serde_json::json!({
                "ready": ready,
                "configuration": {
                    "status": "ready",
                    "path": self.config_path,
                    "exists": self.config_file_exists,
                },
                "data_directory": data_directory.as_json(),
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
            "Awaken doctor\n\n  configuration       ready ({config})\n  data directory      {data_status} ({data_detail})\n  listener            {listener_status} ({listener_detail})\n\nResult: {result}\nNext: {next}\n",
            config = self.config_path.display(),
            data_status = data_directory.status,
            data_detail = data_directory.detail,
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
        // Cause/effect graph: resolved config + data-path metadata + bind result
        // -> independent readiness details -> one overall result. Constraints:
        // diagnostics must not create the absent directory and must not retain
        // the test-bound listener.
        // Decision table: R1 existing directory + free listener -> ready; R2
        // absent child with existing parent + free listener -> ready_to_create;
        // R3 non-directory data path -> blocked; R4 occupied listener -> blocked.
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.bind = "127.0.0.1:0".to_owned();

        let ready: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(ready["ready"], true, "R1");
        assert_eq!(ready["data_directory"]["status"], "ready", "R1");

        let absent = directory.path().join("new").join("data");
        deployment.data_dir = absent.clone();
        let creatable: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(creatable["ready"], true, "R2");
        assert_eq!(
            creatable["data_directory"]["status"], "ready_to_create",
            "R2"
        );
        assert!(!absent.exists(), "doctor remains read-only");

        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, b"fixture").unwrap();
        deployment.data_dir = file;
        let blocked_storage: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(blocked_storage["ready"], false, "R3");
        assert_eq!(blocked_storage["data_directory"]["status"], "blocked", "R3");

        deployment.data_dir = directory.path().to_path_buf();
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        deployment.bind = occupied.local_addr().unwrap().to_string();
        let blocked_listener: serde_json::Value =
            serde_json::from_str(&deployment.doctor_report(true).await).unwrap();
        assert_eq!(blocked_listener["ready"], false, "R4");
        assert_eq!(blocked_listener["listener"]["status"], "blocked", "R4");
    }
}
