//! Redacted rendering of the resolved deployment configuration.

use std::collections::BTreeMap;

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
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  internal bind        {internal_bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  identity mode        {identity}\n  cloud models         {cloud_models}\n  Cloud issuer         {cloud_issuer}\n  OAuth client         {oauth_client}\n  OAuth callback       {oauth_callback}\n  credential source    {credential_source}\n  runtime dispatch     {runtime}\n  Resources backend    {resources}\n  control seal key     {key}\n\nSources: command line --config or standard config.toml, then defaults.\n",
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
