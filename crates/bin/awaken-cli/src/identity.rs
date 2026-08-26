use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_agent_contract::{RedactedString, RedactedStringSource};
use awaken_control::{
    AccountId, LocalBrowserAuth, LocalSetupHandoff, ManagementAuthz, ManagementIdentityMode,
    RemoteManagementAuthz,
};
use awaken_runtime_host::SharedHost;

use crate::config;

fn desktop_oauth_client(
    config: awaken_iam_client::DesktopOAuthConfig,
    cache: awaken_iam_client::CredentialCache,
) -> Result<awaken_iam_client::DesktopOAuthClient, String> {
    // reqwest's blocking client creates and tears down a private Tokio runtime
    // while it is built. Construct it on a dedicated blocking thread so this
    // synchronous IAM adapter is safe no matter whether composition is entered
    // from a service runtime or a CLI command.
    std::thread::spawn(move || {
        awaken_iam_client::DesktopOAuthClient::new(config, cache).map_err(|error| error.to_string())
    })
    .join()
    .map_err(|_| "desktop OAuth client construction panicked".to_owned())?
}

pub(crate) fn ensure_cloud_login<F>(
    config: &config::CloudIamConfig,
    cache: awaken_iam_client::CredentialCache,
    launch: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    if config.access_token.is_some()
        || config.service_token.is_some()
        || config.service_token_file.is_some()
    {
        return Ok(());
    }
    let oauth = desktop_oauth_client(
        awaken_iam_client::DesktopOAuthConfig::new(
            config.issuer.clone(),
            config.oauth_client_id.clone(),
            config.oauth_redirect_uri.clone(),
        ),
        cache,
    )
    .map_err(|error| format!("configure Awaken Cloud login: {error}"))?;
    oauth
        .ensure_credential(launch)
        .map(|_| ())
        .map_err(|error| format!("Awaken Cloud login failed: {error}"))
}

pub(crate) struct IdentityWiring {
    pub(crate) iam: Option<Arc<ManagementAuthz>>,
    pub(crate) remote_iam: Option<Arc<RemoteManagementAuthz>>,
    pub(crate) local_browser_auth: Option<LocalBrowserAuth>,
    pub(crate) local_setup: Option<LocalSetupHandoff>,
    pub(crate) cloud_login: Option<Arc<dyn awaken_admin_config_api::CloudLoginApplication>>,
}

pub(crate) async fn identity_wiring(
    identity_mode: ManagementIdentityMode,
    data_dir: Option<&std::path::Path>,
    org_id: &str,
    iam_workspaces: &[String],
    cloud_iam: &config::CloudIamConfig,
    cloud_credential_cache: awaken_iam_client::CredentialCache,
    entitlement_provider: Option<Box<dyn awaken_iam_core::EntitlementProvider>>,
) -> Result<IdentityWiring, String> {
    let data_dir = data_dir.map(std::path::Path::to_path_buf);
    let org_id = org_id.to_owned();
    let iam_workspaces = iam_workspaces.to_vec();
    let cloud_iam = cloud_iam.clone();
    tokio::task::spawn_blocking(move || {
        identity_wiring_blocking(
            identity_mode,
            data_dir.as_deref(),
            &org_id,
            &iam_workspaces,
            &cloud_iam,
            cloud_credential_cache,
            entitlement_provider,
        )
    })
    .await
    .map_err(|error| format!("identity initialization task failed: {error}"))?
}

fn identity_wiring_blocking(
    identity_mode: ManagementIdentityMode,
    data_dir: Option<&std::path::Path>,
    org_id: &str,
    iam_workspaces: &[String],
    cloud_iam: &config::CloudIamConfig,
    cloud_credential_cache: awaken_iam_client::CredentialCache,
    entitlement_provider: Option<Box<dyn awaken_iam_core::EntitlementProvider>>,
) -> Result<IdentityWiring, String> {
    match identity_mode {
        ManagementIdentityMode::SelfManaged => {
            let dir = data_dir.ok_or_else(|| {
                "self-managed IAM requires a persistent data directory".to_owned()
            })?;
            let workspace = SharedHost::provision_local_workspace_at(dir);
            let iam = match entitlement_provider {
                Some(provider) => awaken_control::embedded_iam_for_tenant_with_entitlements(
                    dir, org_id, &workspace, provider,
                ),
                None => awaken_control::embedded_iam_for_tenant(dir, org_id, &workspace),
            };
            for workspace_id in iam_workspaces {
                iam.register_workspace(workspace_id);
            }
            let account_id = AccountId("local-console-admin".to_owned());
            let (browser, handoff) = iam
                .begin_local_browser(account_id)
                .map_err(|error| format!("local browser authentication: {error}"))?;
            Ok(IdentityWiring {
                iam: Some(iam),
                remote_iam: None,
                local_browser_auth: Some(browser),
                local_setup: Some(handoff),
                cloud_login: None,
            })
        }
        ManagementIdentityMode::AwakenCloud => {
            cloud_identity_wiring(cloud_iam, cloud_credential_cache)
        }
        ManagementIdentityMode::NoLogin => Ok(IdentityWiring {
            iam: None,
            remote_iam: None,
            local_browser_auth: None,
            local_setup: None,
            cloud_login: None,
        }),
    }
}

fn cloud_identity_wiring(
    config: &config::CloudIamConfig,
    cache: awaken_iam_client::CredentialCache,
) -> Result<IdentityWiring, String> {
    let interactive = config.access_token.is_none()
        && config.service_token.is_none()
        && config.service_token_file.is_none();
    let login = interactive
        .then(|| DesktopCloudLogin::new(config, cache))
        .transpose()?
        .map(Arc::new);
    let remote_iam = awaken_cloud_authz(config, login.clone())?;
    Ok(IdentityWiring {
        iam: None,
        remote_iam: Some(remote_iam),
        local_browser_auth: None,
        local_setup: None,
        cloud_login: login
            .map(|login| login as Arc<dyn awaken_admin_config_api::CloudLoginApplication>),
    })
}

struct DesktopCloudLogin {
    oauth: awaken_iam_client::DesktopOAuthClient,
    cache: awaken_iam_client::CredentialCache,
    issuer: String,
    operation: Arc<std::sync::Mutex<()>>,
    running: Arc<AtomicBool>,
    state: Arc<std::sync::Mutex<awaken_admin_config_api::CloudLoginStatusView>>,
}

impl DesktopCloudLogin {
    fn new(
        config: &config::CloudIamConfig,
        cache: awaken_iam_client::CredentialCache,
    ) -> Result<Self, String> {
        let oauth = desktop_oauth_client(
            awaken_iam_client::DesktopOAuthConfig::new(
                config.issuer.clone(),
                config.oauth_client_id.clone(),
                config.oauth_redirect_uri.clone(),
            ),
            cache.clone(),
        )
        .map_err(|error| format!("configure Awaken Cloud login: {error}"))?;
        Ok(Self {
            oauth,
            cache,
            issuer: config.issuer.trim_end_matches('/').to_owned(),
            operation: Arc::new(std::sync::Mutex::new(())),
            running: Arc::new(AtomicBool::new(false)),
            state: Arc::new(std::sync::Mutex::new(cloud_login_status(
                awaken_admin_config_api::CloudLoginState::SignInRequired,
            ))),
        })
    }

    fn credential(&self) -> Result<Option<awaken_iam_client::CachedCredential>, String> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| "Awaken Cloud credential operation is unavailable".to_owned())?;
        if let Some(credential) = self.cache.load(&self.issuer) {
            return Ok(Some(credential));
        }
        // IAM owns refresh semantics, but its desktop client is deliberately
        // blocking. Keep a stale-cache refresh off every Tokio request worker
        // that consumes the synchronous Cloud token-source contract.
        let oauth = self.oauth.clone();
        std::thread::spawn(move || {
            oauth
                .cached_credential()
                .map_err(|error| format!("Awaken Cloud credential refresh failed: {error}"))
        })
        .join()
        .map_err(|_| "Awaken Cloud credential refresh thread panicked".to_owned())?
    }

    fn observed_status(&self) -> awaken_admin_config_api::CloudLoginStatusView {
        if self.cache.load(&self.issuer).is_some() {
            return cloud_login_status(awaken_admin_config_api::CloudLoginState::Authenticated);
        }
        self.state.lock().map_or_else(
            |_| {
                let mut status =
                    cloud_login_status(awaken_admin_config_api::CloudLoginState::Failed);
                status.error_code = Some("cloud_login_state_unavailable".into());
                status
            },
            |status| status.clone(),
        )
    }
}

fn cloud_login_status(
    state: awaken_admin_config_api::CloudLoginState,
) -> awaken_admin_config_api::CloudLoginStatusView {
    awaken_admin_config_api::CloudLoginStatusView {
        state,
        authorize_url: None,
        error_code: None,
    }
}

#[async_trait::async_trait]
impl awaken_admin_config_api::CloudLoginApplication for DesktopCloudLogin {
    async fn status(&self) -> awaken_admin_config_api::CloudLoginStatusView {
        self.observed_status()
    }

    async fn start(&self) -> awaken_admin_config_api::CloudLoginStatusView {
        if self.cache.load(&self.issuer).is_some() {
            return cloud_login_status(awaken_admin_config_api::CloudLoginState::Authenticated);
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.observed_status();
        }
        if let Ok(mut status) = self.state.lock() {
            *status = cloud_login_status(awaken_admin_config_api::CloudLoginState::Authorizing);
        }
        let oauth = self.oauth.clone();
        let operation = Arc::clone(&self.operation);
        let state = Arc::clone(&self.state);
        let running = Arc::clone(&self.running);
        tokio::task::spawn_blocking(move || {
            let result = operation.lock().map_err(|_| ()).and_then(|_guard| {
                oauth
                    .ensure_credential(|url| {
                        let mut status = state
                            .lock()
                            .map_err(|_| "login state unavailable".to_owned())?;
                        status.state = awaken_admin_config_api::CloudLoginState::Authorizing;
                        status.authorize_url = Some(url.to_owned());
                        status.error_code = None;
                        Ok(())
                    })
                    .map_err(|_| ())
            });
            if let Ok(mut status) = state.lock() {
                *status = match result {
                    Ok(_) => {
                        cloud_login_status(awaken_admin_config_api::CloudLoginState::Authenticated)
                    }
                    Err(()) => {
                        let mut failed =
                            cloud_login_status(awaken_admin_config_api::CloudLoginState::Failed);
                        failed.error_code = Some("cloud_login_failed".into());
                        failed
                    }
                };
            }
            running.store(false, Ordering::Release);
        });
        self.observed_status()
    }

    async fn logout(&self) -> Result<(), String> {
        if self.running.load(Ordering::Acquire) {
            return Err("Awaken Cloud login is still in progress".into());
        }
        self.cache
            .clear(&self.issuer)
            .map_err(|error| format!("clear Awaken Cloud login: {error}"))?;
        if let Ok(mut status) = self.state.lock() {
            *status = cloud_login_status(awaken_admin_config_api::CloudLoginState::SignInRequired);
        }
        Ok(())
    }
}

fn awaken_cloud_authz(
    config: &config::CloudIamConfig,
    desktop: Option<Arc<DesktopCloudLogin>>,
) -> Result<Arc<RemoteManagementAuthz>, String> {
    if let Some(path) = &config.service_token_file {
        return RemoteManagementAuthz::connect_with_projected_service_token(
            config.base_url.clone(),
            config.audience.clone(),
            config.issuer.clone(),
            path.clone(),
        );
    }
    let user_token_source: Arc<RedactedStringSource> = match config.access_token.clone() {
        Some(token) => Arc::new(move || Ok(RedactedString::new(token.clone()))),
        None => match desktop {
            Some(desktop) => Arc::new(move || {
                desktop
                    .credential()?
                    .map(|entry| RedactedString::new(entry.token.expose().to_owned()))
                    .ok_or_else(|| "Awaken Cloud login credential is missing or expired".into())
            }),
            None => {
                let issuer = config.issuer.clone();
                Arc::new(move || {
                    awaken_iam_client::CredentialCache::open()
                        .load(&issuer)
                        .map(|entry| RedactedString::new(entry.token.expose().to_owned()))
                        .ok_or_else(|| {
                            "Awaken Cloud login credential is missing or expired".to_string()
                        })
                })
            }
        },
    };
    RemoteManagementAuthz::connect_with_user_token_source(
        config.base_url.clone(),
        config.audience.clone(),
        config.issuer.clone(),
        user_token_source,
        config.service_token.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_iam_client::{
        CachedCredential, CachedOAuthGrant, CredentialCache, RedactedString as IamSecret,
    };
    use awaken_iam_contract::{AccountId as IamAccountId, PrincipalRef};

    fn cloud_config() -> config::CloudIamConfig {
        config::CloudIamConfig {
            base_url: "https://accounts.example".into(),
            inference_base_url: "https://api.example".into(),
            audience: "awaken-runtime".into(),
            issuer: "https://accounts.example".into(),
            oauth_client_id: "awaken-desktop".into(),
            oauth_redirect_uri: "http://127.0.0.1:34115/callback".into(),
            access_token: None,
            developer_key_file: None,
            service_token: None,
            service_token_file: None,
        }
    }

    /// Startup credential decision table:
    ///
    /// | explicit access | service credential | live cache | effect |
    /// | yes | any | any | preserve explicit credential; no OAuth/network |
    /// | no | inline/projected | any | preserve service credential; no desktop login |
    /// | no | no | yes | reuse IAM cache; no browser launch |
    /// | no | no | absent/expired | IAM client owns refresh or interactive PKCE |
    #[test]
    fn existing_credentials_satisfy_cloud_startup_without_browser_login() {
        for mut configured in [
            {
                let mut configured = cloud_config();
                configured.access_token = Some("explicit-access".into());
                configured
            },
            {
                let mut configured = cloud_config();
                configured.service_token = Some("inline-service".into());
                configured
            },
            {
                let mut configured = cloud_config();
                configured.service_token_file = Some("/run/secrets/cloud-token".into());
                configured
            },
        ] {
            configured.issuer = "not a valid issuer".into();
            let directory = tempfile::tempdir().unwrap();
            ensure_cloud_login(
                &configured,
                CredentialCache::at(directory.path().join("credentials.json")),
                |_| panic!("browser must not launch"),
            )
            .unwrap();
        }

        let directory = tempfile::tempdir().unwrap();
        let cache = CredentialCache::at(directory.path().join("credentials.json"));
        let config = cloud_config();
        cache
            .store(
                &config.issuer,
                CachedCredential {
                    token: IamSecret::new("cached-access"),
                    principal: PrincipalRef::Account {
                        account_id: IamAccountId("acct-cached".into()),
                    },
                    expires_at: u64::MAX / 2,
                    oauth: None,
                },
            )
            .unwrap();

        ensure_cloud_login(&config, cache, |_| panic!("browser must not launch")).unwrap();
    }

    /// Runtime identity cause/effect decision table:
    /// C1 canonical cache contains a live account credential -> status and
    /// request token both authenticate; C2 logout while no PKCE operation is in
    /// flight -> clear that same entry and project sign-in-required. Missing or
    /// expired grants enter IAM's tested refresh/interactive rules rather than a
    /// product-owned token path.
    #[test]
    fn desktop_cloud_login_projects_and_clears_the_canonical_cache() {
        use awaken_admin_config_api::CloudLoginApplication as _;

        let directory = tempfile::tempdir().unwrap();
        let cache = CredentialCache::at(directory.path().join("credentials.json"));
        let config = cloud_config();
        cache
            .store(
                &config.issuer,
                CachedCredential {
                    token: IamSecret::new("runtime-access"),
                    principal: PrincipalRef::Account {
                        account_id: IamAccountId("acct-runtime".into()),
                    },
                    expires_at: u64::MAX / 2,
                    oauth: None,
                },
            )
            .unwrap();
        let login = DesktopCloudLogin::new(&config, cache.clone()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(
                login.status().await.state,
                awaken_admin_config_api::CloudLoginState::Authenticated,
                "C1"
            );
            assert_eq!(
                login.credential().unwrap().unwrap().token.expose(),
                "runtime-access",
                "C1"
            );
            login.logout().await.unwrap();
            assert!(cache.load(&config.issuer).is_none(), "C2");
            assert_eq!(
                login.status().await.state,
                awaken_admin_config_api::CloudLoginState::SignInRequired,
                "C2"
            );
        });
    }

    /// Runtime refresh cause/effect decision table:
    ///
    /// | cache entry | OAuth owner | call context | effect |
    /// | live | any | Tokio | return through the cache fast path; no refresh |
    /// | expired | another/none | Tokio | IAM returns interaction-required; no refresh |
    /// | expired | matching | current-thread Tokio | run IAM refresh off-worker and return its typed result |
    ///
    /// R3 is the regression row here: the fixture closes the one discovery
    /// connection so refresh must return a transport error without unwinding
    /// the async runtime. IAM remains the sole refresh/rotation authority; this
    /// test owns only the synchronous token-source to blocking-client boundary.
    #[tokio::test(flavor = "current_thread")]
    async fn stale_cloud_credential_refresh_returns_an_error_without_runtime_panic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });

        let directory = tempfile::tempdir().unwrap();
        let cache = CredentialCache::at(directory.path().join("credentials.json"));
        let mut config = cloud_config();
        config.issuer = issuer.clone();
        cache
            .store(
                &issuer,
                CachedCredential {
                    token: IamSecret::new("expired-runtime-access"), // awaken-allow: secret
                    principal: PrincipalRef::Account {
                        account_id: IamAccountId("acct-expired-runtime".into()),
                    },
                    expires_at: 0,
                    oauth: Some(CachedOAuthGrant {
                        refresh_token: IamSecret::new("runtime-refresh"), // awaken-allow: secret
                        client_id: config.oauth_client_id.clone(),
                        scopes: vec!["openid".into()],
                    }),
                },
            )
            .unwrap();
        let login = tokio::task::spawn_blocking({
            let cache = cache.clone();
            move || DesktopCloudLogin::new(&config, cache)
        })
        .await
        .unwrap()
        .unwrap();

        let error = login.credential().unwrap_err();
        server.join().unwrap();
        assert!(
            error.contains("Awaken Cloud credential refresh failed"),
            "R3: {error}"
        );
        assert!(cache.load(&issuer).is_none(), "R3");
        assert_eq!(
            cache.load_entry(&issuer).unwrap().token.expose(),
            "expired-runtime-access",
            "R3"
        );
    }

    /// Cause/effect decision rule: C1 interactive Cloud identity is assembled
    /// while a Tokio service runtime is already polling; E1 the blocking OAuth
    /// client is constructed on the dedicated blocking pool and initialization
    /// returns through its ordinary Result channel without a nested-runtime
    /// panic. K1 `identity_wiring` remains the sole IAM composition owner; this
    /// test does not create a test-only client path or enlarge the runtime stack.
    /// D1=C1=>E1, with the unreachable fixture issuer selecting the typed JWKS
    /// failure outcome after the blocking boundary has been crossed.
    #[tokio::test(flavor = "multi_thread")]
    async fn interactive_cloud_identity_initializes_outside_the_async_poll_stack() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CredentialCache::at(directory.path().join("credentials.json"));
        let config = cloud_config();
        cache
            .store(
                &config.issuer,
                CachedCredential {
                    token: IamSecret::new("async-runtime-access"),
                    principal: PrincipalRef::Account {
                        account_id: IamAccountId("acct-async-runtime".into()),
                    },
                    expires_at: u64::MAX / 2,
                    oauth: None,
                },
            )
            .unwrap();
        let error = match identity_wiring(
            ManagementIdentityMode::AwakenCloud,
            None,
            "org-test",
            &[],
            &config,
            cache,
            None,
        )
        .await
        {
            Ok(_) => panic!("the unreachable fixture issuer must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("JWKS fetch failed"), "D1: {error}");
    }
}
