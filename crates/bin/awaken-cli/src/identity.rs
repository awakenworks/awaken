use std::sync::Arc;

use awaken_agent_contract::{RedactedString, RedactedStringSource};
use awaken_control::{
    AccountId, LocalBrowserAuth, LocalSetupHandoff, ManagementAuthz, ManagementIdentityMode,
    RemoteManagementAuthz,
};
use awaken_runtime_host::SharedHost;

use crate::config;

pub(crate) struct IdentityWiring {
    pub(crate) iam: Option<Arc<ManagementAuthz>>,
    pub(crate) remote_iam: Option<Arc<RemoteManagementAuthz>>,
    pub(crate) local_browser_auth: Option<LocalBrowserAuth>,
    pub(crate) local_setup: Option<LocalSetupHandoff>,
}

pub(crate) fn identity_wiring(
    identity_mode: ManagementIdentityMode,
    data_dir: Option<&std::path::Path>,
    org_id: &str,
    iam_workspaces: &[String],
    cloud_iam: &config::CloudIamConfig,
) -> Result<IdentityWiring, String> {
    match identity_mode {
        ManagementIdentityMode::SelfManaged => {
            let dir = data_dir.ok_or_else(|| {
                "self-managed IAM requires a persistent data directory".to_owned()
            })?;
            let workspace = SharedHost::provision_local_workspace_at(dir);
            let iam = awaken_control::embedded_iam_for_tenant(dir, org_id, &workspace);
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
            })
        }
        ManagementIdentityMode::AwakenCloud => Ok(IdentityWiring {
            iam: None,
            remote_iam: Some(awaken_cloud_authz(cloud_iam)?),
            local_browser_auth: None,
            local_setup: None,
        }),
        ManagementIdentityMode::NoLogin => Ok(IdentityWiring {
            iam: None,
            remote_iam: None,
            local_browser_auth: None,
            local_setup: None,
        }),
    }
}

fn awaken_cloud_authz(
    config: &config::CloudIamConfig,
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
        None => {
            let base_url = config.base_url.clone();
            Arc::new(move || {
                awaken_iam_client::CredentialCache::open()
                    .load(&base_url)
                    .map(|entry| RedactedString::new(entry.token.expose().to_owned()))
                    .ok_or_else(|| {
                        "Awaken Cloud login credential is missing or expired".to_string()
                    })
            })
        }
    };
    RemoteManagementAuthz::connect_with_user_token_source(
        config.base_url.clone(),
        config.audience.clone(),
        config.issuer.clone(),
        user_token_source,
        config.service_token.clone(),
    )
}
