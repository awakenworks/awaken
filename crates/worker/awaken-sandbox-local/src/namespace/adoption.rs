//! Durable Namespace-handle adoption and exact control-topology recovery.

use super::*;

impl NamespaceProvider {
    /// Re-open a namespace sandbox from its durable handle so companion
    /// capabilities operate on the Run's existing environment.
    pub async fn adopt_sandbox(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        self.adopt_sandbox_with_requested_control_services(handle, None)
            .await
    }

    /// Runtime-Host adoption path which reprojects the exact typed control
    /// demand frozen for this Session. The durable handle remains topology-only.
    pub async fn adopt_sandbox_with_control_services(
        &self,
        handle: &pc::SandboxHandle,
        control_services: &std::collections::BTreeSet<SandboxControlServiceKind>,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        self.adopt_sandbox_with_requested_control_services(handle, Some(control_services))
            .await
    }

    async fn adopt_sandbox_with_requested_control_services(
        &self,
        handle: &pc::SandboxHandle,
        requested: Option<&std::collections::BTreeSet<SandboxControlServiceKind>>,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        let provider_kind = if cfg!(target_os = "macos") {
            pc::NamespaceProviderKind::Seatbelt
        } else {
            pc::NamespaceProviderKind::Bubblewrap
        };
        let payload = handle.namespace_payload(provider_kind)?;
        let control_services = pc::validate_adopted_sandbox_control_services(
            requested,
            &payload.control_services,
            &Self::capabilities(),
        )
        .map_err(err)?;
        let outputs_path = payload.outputs_path.clone();
        let raw_root = crate::sandbox_dir(&self.base, &handle.sandbox_id);
        let root = IsolatedRoot::new(std::fs::canonicalize(&raw_root).unwrap_or(raw_root));
        let host_workspace = root.resolve("/workspace").map_err(err)?;
        let host_outputs = root.resolve(&outputs_path).map_err(err)?;
        let control_directory =
            if control_services.contains(&SandboxControlServiceKind::RepositoryGitCredential) {
                Some(private_control_host_directory(&root)?)
            } else {
                None
            };
        let layout = control_directory
            .iter()
            .cloned()
            .map(private_control_mount)
            .collect();
        Ok(NamespaceSandbox {
            id: handle.sandbox_id.clone(),
            root,
            outputs_path,
            host_workspace,
            host_outputs,
            base_env: payload.base_env.clone(),
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network: payload.network.clone(),
            control_services,
            control_directory,
            control_publication: Arc::new(NamespaceControlPublicationRegistry::default()),
            layout: std::sync::RwLock::new(layout),
            realized: Vec::new(),
            secret_paths: Vec::new(),
            memory_mounts: std::sync::Mutex::new(Vec::new()),
            memory_mounter: self.memory_mounter.clone(),
            adopted_handle: Some(handle.clone()),
        })
    }
}
