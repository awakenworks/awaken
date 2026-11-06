// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This source implements the repository-controlled remote worker product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Package images built by the Docker Engine over its HTTP API (bollard).

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use base64::Engine as _;
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::image::{BuildImageOptions, CreateImageOptions, PruneImagesOptions, PushImageOptions};
use futures_util::StreamExt;

use crate::engine_build::{PACKAGE_BUILD_TIMEOUT, PackageBuildNetwork, backend};

/// An Engine API response keeps the class its HTTP status proves; every other
/// failure is a retryable mechanism failure.
fn engine(error: bollard::errors::Error) -> RuntimeError {
    match error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => RuntimeError::from_api_status(
            status_code,
            format!("Docker responded with status code {status_code}: {message}"),
        ),
        bollard::errors::Error::DockerStreamError { error } => RuntimeError::Backend(error),
        other => RuntimeError::Backend(other.to_string()),
    }
}

/// Render the shared build-network decision in Docker's `NetworkMode` vocabulary.
fn package_build_network(
    network: &pc::NetworkPolicy,
    host_opt_in: bool,
) -> Result<&'static str, RuntimeError> {
    Ok(
        match crate::engine_build::package_build_network(network, host_opt_in)? {
            PackageBuildNetwork::EngineDefault => "default",
            PackageBuildNetwork::Host => "host",
            PackageBuildNetwork::Denied => "none",
        },
    )
}

#[derive(serde::Deserialize)]
struct RegistryAuthFile {
    #[serde(default)]
    auths: HashMap<String, RegistryAuthEntry>,
}

#[derive(serde::Deserialize)]
struct RegistryAuthEntry {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default, rename = "identitytoken")]
    identitytoken: Option<String>,
    #[serde(default, rename = "registrytoken")]
    registrytoken: Option<String>,
}

fn registry_key_matches(key: &str, registry: &str) -> bool {
    fn normalize(value: &str) -> &str {
        value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .unwrap_or(value)
            .trim_end_matches('/')
    }
    normalize(key) == normalize(registry)
}

/// Builds content-addressed package images on a Docker Engine, optionally
/// publishing them to a shared registry.
pub struct DockerPackageImageProvisioner {
    docker: Docker,
    /// Serialize the cache-probe/build sequence so concurrent Sessions with the
    /// same Environment cannot race two identical immutable image builds.
    package_builds: tokio::sync::Mutex<()>,
    package_registry: Option<String>,
    package_registry_credentials: Option<DockerCredentials>,
    package_cache_ttl: Option<std::time::Duration>,
    package_build_timeout: std::time::Duration,
    package_build_host_network: bool,
}

impl DockerPackageImageProvisioner {
    /// Connect using the local defaults (unix socket / named pipe / env).
    pub fn connect_local() -> Result<Self, RuntimeError> {
        Ok(Self::with_client(
            Docker::connect_with_local_defaults().map_err(engine)?,
        ))
    }

    /// Wrap an already-built client.
    #[must_use]
    pub fn with_client(docker: Docker) -> Self {
        Self {
            docker,
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_credentials: None,
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

    /// Bound unused Awaken-derived images in the local engine cache. Docker's
    /// prune operation never removes an image referenced by a container.
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

    /// Load the selected registry entry from a Docker/containers authentication
    /// file. Only the builder's Docker API receives these credentials.
    pub fn with_package_registry_auth_file(
        mut self,
        path: impl AsRef<Path>,
    ) -> Result<Self, RuntimeError> {
        let registry = self
            .package_registry
            .as_deref()
            .ok_or_else(|| backend("registry authentication requires a package registry"))?;
        let bytes = std::fs::read(path.as_ref()).map_err(backend)?;
        let config: RegistryAuthFile = serde_json::from_slice(&bytes).map_err(backend)?;
        let (serveraddress, entry) = config
            .auths
            .into_iter()
            .find(|(server, _)| registry_key_matches(server, registry))
            .ok_or_else(|| {
                backend(format!(
                    "registry authentication file has no entry for `{registry}`"
                ))
            })?;
        let (decoded_username, decoded_password) = entry
            .auth
            .as_deref()
            .and_then(|auth| base64::engine::general_purpose::STANDARD.decode(auth).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|value| {
                value
                    .split_once(':')
                    .map(|(username, password)| (username.to_owned(), password.to_owned()))
            })
            .unzip();
        self.package_registry_credentials = Some(DockerCredentials {
            username: entry.username.or(decoded_username),
            password: entry.password.or(decoded_password),
            auth: entry.auth,
            identitytoken: entry.identitytoken,
            registrytoken: entry.registrytoken,
            serveraddress: Some(serveraddress),
            ..Default::default()
        });
        Ok(self)
    }

    async fn resolve_package_build(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
    ) -> Result<(String, String, String, String), RuntimeError> {
        let base = self
            .docker
            .inspect_image(base_image)
            .await
            .map_err(engine)?;
        let base_identity = base
            .id
            .ok_or_else(|| backend("docker returned an empty base-image identity"))?;
        if base_identity.is_empty() {
            return Err(backend("docker returned an empty base-image identity"));
        }
        let base_user = base
            .config
            .and_then(|config| config.user)
            .unwrap_or_default();
        let (dockerfile, fingerprint) =
            awaken_container_contract::package_image_recipe(&base_identity, &base_user, packages)?;
        let repository = self.package_registry.as_ref().map_or_else(
            || "awaken-packages".to_string(),
            |registry| format!("{registry}/awaken-packages"),
        );
        let image = format!("{repository}:{fingerprint}");
        Ok((dockerfile, fingerprint, repository, image))
    }

    async fn prune_package_cache(&self) {
        let Some(ttl) = self.package_cache_ttl else {
            return;
        };
        let filters = [
            ("dangling".to_owned(), vec!["false".to_owned()]),
            (
                "label".to_owned(),
                vec!["org.awaken.package-recipe".to_owned()],
            ),
            ("until".to_owned(), vec![format!("{}s", ttl.as_secs())]),
        ]
        .into_iter()
        .collect();
        let _ = self
            .docker
            .prune_images(Some(PruneImagesOptions { filters }))
            .await;
    }

    async fn package_image_reference(
        &self,
        image: &str,
        repository: &str,
    ) -> Result<String, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(image.to_string());
        }
        self.docker
            .inspect_image(image)
            .await
            .map_err(engine)?
            .repo_digests
            .unwrap_or_default()
            .into_iter()
            .find(|digest| digest.starts_with(&format!("{repository}@sha256:")))
            .ok_or_else(|| backend("package image has no immutable repository digest"))
    }

    async fn push(&self, repository: &str, fingerprint: &str) -> Result<(), RuntimeError> {
        let mut push = self.docker.push_image(
            repository,
            Some(PushImageOptions {
                tag: fingerprint.to_owned(),
            }),
            self.package_registry_credentials.clone(),
        );
        while let Some(result) = push.next().await {
            result.map_err(engine)?;
        }
        Ok(())
    }

    async fn pulled(&self, from_image: String, tag: String) -> bool {
        let mut pull = self.docker.create_image(
            Some(CreateImageOptions {
                from_image,
                tag,
                ..Default::default()
            }),
            None,
            self.package_registry_credentials.clone(),
        );
        while let Some(result) = pull.next().await {
            if result.is_err() {
                return false;
            }
        }
        true
    }

    async fn build_package_image(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        let _build_guard = self.package_builds.lock().await;
        self.prune_package_cache().await;
        let (dockerfile, fingerprint, repository, image) =
            self.resolve_package_build(base_image, packages).await?;
        if self.package_registry.is_none() && self.docker.inspect_image(&image).await.is_ok() {
            return self.package_image_reference(&image, &repository).await;
        }
        if self.package_registry.is_some() {
            if self.pulled(repository.clone(), fingerprint.clone()).await
                && self.docker.inspect_image(&image).await.is_ok()
            {
                return self.package_image_reference(&image, &repository).await;
            }
            // A local cache hit is not evidence that another Worker can pull the
            // image. Repair an empty/expired registry from the deterministic local
            // tag before returning a shared digest.
            if self.docker.inspect_image(&image).await.is_ok() {
                self.push(&repository, &fingerprint).await?;
                return self.package_image_reference(&image, &repository).await;
            }
        }

        let mut context = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(dockerfile.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        context
            .append_data(&mut header, "Containerfile", dockerfile.as_bytes())
            .map_err(backend)?;
        let context = context.into_inner().map_err(backend)?;
        let options = BuildImageOptions::<String> {
            dockerfile: "Containerfile".into(),
            t: image.clone(),
            rm: true,
            forcerm: true,
            networkmode: package_build_network(network, self.package_build_host_network)?.into(),
            ..Default::default()
        };
        let mut build = self.docker.build_image(options, None, Some(context.into()));
        while let Some(result) = build.next().await {
            result.map_err(engine)?;
        }
        self.docker.inspect_image(&image).await.map_err(engine)?;
        if self.package_registry.is_some() {
            self.push(&repository, &fingerprint).await?;
            return self.package_image_reference(&image, &repository).await;
        }
        Ok(image)
    }
}

#[async_trait]
impl awaken_container_contract::PackageImageProvisioner for DockerPackageImageProvisioner {
    async fn probe_builder(&self) -> Result<(), RuntimeError> {
        self.docker.version().await.map(|_| ()).map_err(engine)
    }

    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        self.docker
            .inspect_image(reference)
            .await
            .map_err(engine)?
            .id
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| backend("docker returned an empty base-image identity"))
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
            return Ok(self.docker.inspect_image(image).await.is_ok());
        }
        if !self.pulled(image.to_string(), String::new()).await {
            return Ok(false);
        }
        Ok(self.docker.inspect_image(image).await.is_ok())
    }
}

#[cfg(test)]
mod tests;
