//! Redacted rendering of the resolved deployment configuration.

use std::collections::BTreeMap;

use awaken_runtime_host::DispatchBackend;

use super::ResolvedDeployment;

impl ResolvedDeployment {
    pub fn report(&self, json: bool) -> String {
        let resource_backend = if self.resources.is_shared() {
            "postgres"
        } else {
            "embedded"
        };
        let runtime_backend = match self.runtime.dispatch_backend {
            DispatchBackend::Sqlite => "sqlite",
            DispatchBackend::Postgres => "postgres",
        };
        let databases = BTreeMap::from([
            ("catalog", render_store_backend(&self.control.catalog)),
            ("credential", render_store_backend(&self.control.credential)),
            ("config", render_store_backend(&self.control.config)),
            ("admin", render_store_backend(&self.control.admin)),
            ("sessions", render_store_backend(&self.control.sessions)),
        ]);
        if json {
            return serde_json::to_string_pretty(&serde_json::json!({
                "role": self.role.as_str(),
                "mode": self.mode.as_str(),
                "bind": self.bind,
                "data_dir": self.data_dir,
                "config_file": self.config_path,
                "config_file_exists": self.config_file_exists,
                "no_browser": self.no_browser,
                "run_local_pool": self.run_local_pool,
                "identity_mode": identity_mode_name(self.identity_mode),
                "cloud_models": self.cloud_models.as_str(),
                "runtime_dispatch_backend": runtime_backend,
                "resource_backend": resource_backend,
                "control_databases": databases,
                "database_migrations": "automatic at startup",
                "seal_key": self.seal_key.description(),
                "origins": self.origins,
                "deprecations": self.deprecations,
            }))
            .expect("configuration report is serializable");
        }
        let mut report = format!(
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  identity mode        {identity}\n  cloud models         {cloud_models}\n  runtime dispatch     {runtime}\n  resource plane       {resources}\n  control seal key     {key}\n\nSources: command line --config or standard config.toml, then defaults.\n",
            role = self.role.as_str(),
            mode = self.mode.as_str(),
            bind = self.bind,
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
            runtime = runtime_backend,
            resources = resource_backend,
            key = self.seal_key.description(),
        );
        report.push_str("\nControl databases (migrations run automatically at startup)\n");
        for (name, backend) in databases {
            report.push_str(&format!("  {name:<20} {backend}\n"));
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
