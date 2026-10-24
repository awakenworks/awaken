// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This source implements the repository-controlled remote worker product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Package images built by rootless Podman over its CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_container_contract::RuntimeError;
use awaken_local_process::podman::{CommandExec, OsCommandExec};
use awaken_provisioning_contract as pc;

use crate::engine_build::{PACKAGE_BUILD_TIMEOUT, PackageBuildNetwork, backend};

/// Builds content-addressed package images with the deployment-selected
/// Podman executable, optionally publishing them to a shared registry.
pub struct PodmanPackageImageProvisioner {
    bin: String,
    exec: Arc<dyn CommandExec>,
    /// Serialize the cache-probe/build sequence so concurrent Sessions with the
    /// same Environment cannot race two identical immutable image builds.
    package_builds: tokio::sync::Mutex<()>,
    package_registry: Option<String>,
    package_registry_auth_file: Option<PathBuf>,
    package_cache_ttl: Option<std::time::Duration>,
    package_build_timeout: std::time::Duration,
    package_build_host_network: bool,
}

impl PodmanPackageImageProvisioner {
    /// Use the deployment-selected Podman executable.
    #[must_use]
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self::with_exec(bin, Arc::new(OsCommandExec))
    }

    fn with_exec(bin: impl Into<String>, exec: Arc<dyn CommandExec>) -> Self {
        Self {
            bin: bin.into(),
            exec,
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_auth_file: None,
            package_cache_ttl: None,
            package_build_timeout: PACKAGE_BUILD_TIMEOUT,
            package_build_host_network: false,
        }
    }

    /// Deployment-owned selection; the Environment's network policy still bounds it.
    #[must_use]
    pub fn with_package_build_host_network(mut self, enabled: bool) -> Self {
        self.package_build_host_network = enabled;
        self
    }

    /// Publish content-addressed package images to a shared OCI registry.
    #[must_use]
    pub fn with_package_registry(mut self, registry: impl Into<String>) -> Self {
        self.package_registry = Some(registry.into().trim_end_matches('/').to_string());
        self
    }

    /// Bound unused Awaken-derived images in the local rootless engine cache.
    #[must_use]
    pub fn with_package_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.package_cache_ttl = Some(ttl);
        self
    }

    /// Bound one registry lookup/build/push transaction. A timeout fails the
    /// Environment activation closed; it never selects the unmodified base image.
    #[must_use]
    pub fn with_package_build_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.package_build_timeout = timeout;
        self
    }

    /// Select a Docker/containers authentication file for registry pull/push.
    /// Podman reads it only in the builder process.
    pub fn with_package_registry_auth_file(
        mut self,
        path: impl AsRef<Path>,
    ) -> Result<Self, RuntimeError> {
        if self.package_registry.is_none() {
            return Err(backend(
                "registry authentication requires a package registry",
            ));
        }
        let path = path.as_ref();
        if !path.is_file() {
            return Err(backend(format!(
                "registry authentication file `{}` is not a file",
                path.display()
            )));
        }
        self.package_registry_auth_file = Some(path.to_owned());
        Ok(self)
    }

    /// Run a podman subcommand, returning trimmed stdout (or a backend error).
    async fn run(&self, args: &[String]) -> Result<String, RuntimeError> {
        let out = self.exec.exec(&self.bin, args).await.map_err(backend)?;
        if !out.ok {
            return Err(RuntimeError::Backend(format!(
                "podman {}: {}",
                args.first().cloned().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn resolve_package_build(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
    ) -> Result<(String, String, String), RuntimeError> {
        let base_identity = self
            .run(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}\n{{.Config.User}}".into(),
                base_image.into(),
            ])
            .await?;
        let mut base_lines = base_identity.lines();
        let base_identity = base_lines.next().unwrap_or_default();
        let base_user = base_lines.next().unwrap_or_default();
        if base_identity.is_empty() {
            return Err(backend("podman returned an empty base-image identity"));
        }
        let (containerfile, fingerprint) =
            awaken_container_contract::package_image_recipe(base_identity, base_user, packages)?;
        let image = self.package_registry.as_ref().map_or_else(
            || format!("localhost/awaken-packages:{fingerprint}"),
            |registry| format!("{registry}/awaken-packages:{fingerprint}"),
        );
        Ok((containerfile, fingerprint, image))
    }

    async fn prune_package_cache(&self) {
        let Some(ttl) = self.package_cache_ttl else {
            return;
        };
        let _ = self
            .run(&[
                "image".into(),
                "prune".into(),
                "--force".into(),
                "--all".into(),
                "--filter".into(),
                "label=org.awaken.package-recipe".into(),
                "--filter".into(),
                format!("until={}s", ttl.as_secs()),
            ])
            .await;
    }

    fn registry_command(&self, command: &str, image: &str) -> Vec<String> {
        let mut args = vec![command.to_owned()];
        if let Some(path) = &self.package_registry_auth_file {
            args.extend(["--authfile".to_owned(), path.to_string_lossy().into_owned()]);
        }
        args.push(image.to_owned());
        args
    }

    async fn package_image_reference(&self, image: &str) -> Result<String, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(image.to_string());
        }
        let digest = self
            .run(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{index .RepoDigests 0}}".into(),
                image.to_string(),
            ])
            .await?;
        if digest.contains("@sha256:") {
            Ok(digest)
        } else {
            Err(backend("package image has no immutable repository digest"))
        }
    }

    async fn image_exists(&self, image: &str) -> bool {
        self.run(&["image".into(), "exists".into(), image.to_string()])
            .await
            .is_ok()
    }

    async fn build_package_image(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        let _build_guard = self.package_builds.lock().await;
        self.prune_package_cache().await;
        let (containerfile, fingerprint, image) =
            self.resolve_package_build(base_image, packages).await?;
        if self.package_registry.is_none() && self.image_exists(&image).await {
            return self.package_image_reference(&image).await;
        }
        if self.package_registry.is_some()
            && self
                .run(&self.registry_command("pull", &image))
                .await
                .is_ok()
        {
            return self.package_image_reference(&image).await;
        }
        // A local cache hit is not evidence that another Worker can pull the
        // image. Repair an empty/expired registry from the deterministic local
        // tag before returning a shared digest.
        if self.package_registry.is_some() && self.image_exists(&image).await {
            self.run(&self.registry_command("push", &image)).await?;
            return self.package_image_reference(&image).await;
        }
        let context = tempfile::Builder::new()
            .prefix(&format!("awaken-package-{fingerprint}-"))
            .tempdir()
            .map_err(backend)?;
        let containerfile_path = context.path().join("Containerfile");
        std::fs::write(&containerfile_path, containerfile).map_err(backend)?;
        let mut build = vec!["build".into()];
        // Render the shared build-network decision in Podman's CLI vocabulary.
        match crate::engine_build::package_build_network(network, self.package_build_host_network)?
        {
            PackageBuildNetwork::EngineDefault => {}
            PackageBuildNetwork::Host => {
                build.extend(["--network".into(), "host".into()]);
            }
            PackageBuildNetwork::Denied => {
                build.extend(["--network".into(), "none".into()]);
            }
        }
        build.extend([
            "--tag".into(),
            image.clone(),
            "--file".into(),
            containerfile_path.to_string_lossy().into_owned(),
            context.path().to_string_lossy().into_owned(),
        ]);
        self.run(&build).await?;
        if self.package_registry.is_some() {
            self.run(&self.registry_command("push", &image)).await?;
            return self.package_image_reference(&image).await;
        }
        Ok(image)
    }
}

#[async_trait]
impl awaken_container_contract::PackageImageProvisioner for PodmanPackageImageProvisioner {
    async fn probe_builder(&self) -> Result<(), RuntimeError> {
        self.run(&["info".into(), "--format".into(), "{{.Host.Arch}}".into()])
            .await
            .map(|_| ())
    }

    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        self.run(&[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            reference.into(),
        ])
        .await
        .and_then(|identity| {
            (!identity.is_empty())
                .then_some(identity)
                .ok_or_else(|| backend("podman returned an empty base-image identity"))
        })
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            return Ok(base_image.to_string());
        }
        tokio::time::timeout(
            self.package_build_timeout,
            self.build_package_image(base_image, packages, network),
        )
        .await
        .map_err(|_| backend("package image preparation exceeded its deadline"))?
    }

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &awaken_environment_contract::PackageRequirements,
        _network: &pc::NetworkPolicy,
        image: &str,
    ) -> Result<bool, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(self.image_exists(image).await);
        }
        Ok(self
            .run(&self.registry_command("pull", image))
            .await
            .is_ok())
    }
}

#[cfg(test)]
mod tests;
