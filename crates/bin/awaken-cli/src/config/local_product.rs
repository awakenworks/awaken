use std::fs;

use awaken_runtime_host::AcpWorkerProfile;

use super::ResolvedDeployment;

impl ResolvedDeployment {
    pub fn ensure_data_layout(&self) -> Result<(), String> {
        for path in [
            self.data_dir.clone(),
            self.data_dir.join("runtime"),
            self.data_dir.join("sandboxes"),
            self.data_dir.join("logs"),
        ] {
            fs::create_dir_all(&path)
                .map_err(|error| format!("create data directory {}: {error}", path.display()))?;
        }
        Ok(())
    }

    /// Apply the canonical host-discovery result to the one runtime profile.
    /// Explicit `acp_clis` constrain the discovered set; an unconfigured Local
    /// install advertises every detected catalog row. Missing/broken CLIs are
    /// retained only in the diagnostic read model and never in Worker routes.
    pub fn apply_local_acp_observations(
        &mut self,
        observations: Vec<awaken_acp_application::AcpHostObservation>,
        routable_cli_ids: Vec<String>,
    ) -> Result<(), String> {
        self.runtime.acp = if routable_cli_ids.is_empty() {
            None
        } else {
            Some(AcpWorkerProfile::new(routable_cli_ids)?)
        };
        self.local_acp_observations = observations;
        Ok(())
    }
}
