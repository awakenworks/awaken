//! Container-only projection of sandbox-neutral paths and launch environment.
//!
//! Resource staging deliberately stores paths relative to a sandbox root, while
//! OCI bind destinations must be absolute. Likewise, the launch resolver owns a
//! host config-home path that must never cross into a container. These are backend
//! translations, not resource-domain or configuration decisions.

use awaken_provisioning_contract::MountRequirement;
use awaken_run_executor_acp::{AcpCli, AcpLaunch};

use super::{SANDBOX_CONFIG_HOME, SANDBOX_WORKSPACE};

pub(super) fn resource_mounts(mounts: Vec<MountRequirement>) -> Vec<MountRequirement> {
    mounts
        .into_iter()
        .map(|mut mount| {
            if !mount.mount_path.starts_with('/') {
                mount.mount_path = format!(
                    "{SANDBOX_WORKSPACE}/{}",
                    mount.mount_path.trim_start_matches('/')
                );
            }
            mount
        })
        .collect()
}

pub(super) fn config_home(launch: &mut AcpLaunch, cli: &AcpCli) {
    launch.env.retain(|(key, _)| key != cli.config_home_env);
    launch.env.push((
        cli.config_home_env.to_string(),
        SANDBOX_CONFIG_HOME.to_string(),
    ));
}

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract as pc;

    use super::*;

    fn mount(path: &str) -> MountRequirement {
        MountRequirement {
            mount_id: path.into(),
            source: pc::MountSource::Inline {
                contents: "value".into(),
            },
            mount_path: path.into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }
    }

    #[test]
    fn relative_resource_paths_are_projected_below_the_container_workspace() {
        let projected = resource_mounts(vec![
            mount(".mnt/input.txt"),
            mount("/acp-config/config.toml"),
        ]);
        assert_eq!(projected[0].mount_path, "/workspace/.mnt/input.txt");
        assert_eq!(projected[1].mount_path, "/acp-config/config.toml");
    }

    #[test]
    fn container_config_home_replaces_the_host_path() {
        let cli = awaken_run_executor_acp::acp_cli("gemini").unwrap();
        let mut launch = AcpLaunch::custom(
            vec!["gemini".into()],
            vec![("GEMINI_DIR".into(), "/host/private/config".into())],
        );
        config_home(&mut launch, cli);
        assert_eq!(
            launch.env,
            vec![("GEMINI_DIR".into(), "/acp-config".into())]
        );
    }
}
