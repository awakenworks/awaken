//! Runtime-owned process environment shared by native exec, Hand, and ACP agents.

use super::*;

/// Runtime-owned configuration homes which may contain provider-created
/// credential caches. Checkpoint decorators consume this exact contract instead
/// of copying process-environment defaults.
#[must_use]
pub fn runtime_configuration_homes() -> &'static [&'static str] {
    &[
        pc::WorkspaceLayout::ACP_CONFIG_ROOT,
        pc::WorkspaceLayout::CODEX_CONFIG_ROOT,
        pc::WorkspaceLayout::XDG_CONFIG_ROOT,
    ]
}

fn workspace_scoped_path(value: &str) -> bool {
    let path = std::path::Path::new(value);
    path.is_absolute()
        && path.starts_with(pc::WorkspaceLayout::ROOT)
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

fn selected_workspace_path<'a>(
    values: impl DoubleEndedIterator<Item = &'a str>,
    default: &str,
) -> String {
    values
        .rev()
        .find(|value| workspace_scoped_path(value))
        .unwrap_or(default)
        .to_string()
}

/// Keep the caller's last explicit workspace-scoped path, otherwise install
/// the runtime default. All duplicates are collapsed so backend argv ordering
/// cannot silently change the selected process home.
fn bind_workspace_path(command: &mut pc::Command, name: &str, default: &str) {
    let selected = selected_workspace_path(
        command.env.iter().filter_map(|var| {
            if var.name != name {
                return None;
            }
            let pc::EnvValue::Inline { value } = &var.value else {
                return None;
            };
            Some(value.as_str())
        }),
        default,
    );
    command.env.retain(|var| var.name != name);
    command.env.push(pc::EnvVar {
        name: name.into(),
        value: pc::EnvValue::Inline { value: selected },
        visibility: pc::EnvVisibility::Process,
    });
}

pub(super) fn bind_resident_process_environment(
    env: &mut Vec<(String, String)>,
    outputs_path: &str,
) {
    env.retain(|(name, _)| !matches!(name.as_str(), "AWAKEN_PROJECT_DIR" | "AWAKEN_OUTPUTS_DIR"));
    env.extend([
        (
            "AWAKEN_PROJECT_DIR".into(),
            pc::WorkspaceLayout::ROOT.into(),
        ),
        ("AWAKEN_OUTPUTS_DIR".into(), outputs_path.into()),
    ]);
    for (name, default) in [
        ("HOME", pc::WorkspaceLayout::ROOT),
        ("XDG_CONFIG_HOME", pc::WorkspaceLayout::XDG_CONFIG_ROOT),
        ("XDG_CACHE_HOME", pc::WorkspaceLayout::XDG_CACHE_ROOT),
    ] {
        let selected = selected_workspace_path(
            env.iter()
                .filter_map(|(candidate, value)| (candidate == name).then_some(value.as_str())),
            default,
        );
        env.retain(|(candidate, _)| candidate != name);
        env.push((name.into(), selected));
    }
}

/// Portable PID-1 command for a Session-owned container environment. Attempt
/// commands run through exec; PID 1 only keeps the mount and network namespaces
/// alive until the owning Session disposes the sandbox.
pub(super) fn environment_keepalive_command() -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done".into(),
    ]
}

impl<R: ContainerRuntime + 'static> ContainerSandbox<R> {
    pub(super) async fn materialize_command(
        &self,
        mut command: pc::Command,
    ) -> Result<pc::MaterializedCommand, pc::SandboxError> {
        // Container processes share the same stable interior paths. Bind the
        // runtime-owned workspace, outputs, and user directories at this common
        // process boundary so native exec, the long-lived Hand, ACP agents, and
        // adopted environments cannot diverge. HOME and the XDG roots must live
        // under the writable workspace because hardened backends expose a
        // read-only image root; browsers and toolchains otherwise fail while
        // trying to initialize `/root`.
        command.env.retain(|var| {
            !matches!(
                var.name.as_str(),
                "AWAKEN_PROJECT_DIR" | "AWAKEN_OUTPUTS_DIR"
            )
        });
        command.env.extend([
            pc::EnvVar {
                name: "AWAKEN_PROJECT_DIR".into(),
                value: pc::EnvValue::Inline {
                    value: pc::WorkspaceLayout::ROOT.into(),
                },
                visibility: pc::EnvVisibility::Process,
            },
            pc::EnvVar {
                name: "AWAKEN_OUTPUTS_DIR".into(),
                value: pc::EnvValue::Inline {
                    value: self.outputs_path.clone(),
                },
                visibility: pc::EnvVisibility::Process,
            },
        ]);
        bind_workspace_path(&mut command, "HOME", pc::WorkspaceLayout::ROOT);
        bind_workspace_path(
            &mut command,
            "XDG_CONFIG_HOME",
            pc::WorkspaceLayout::XDG_CONFIG_ROOT,
        );
        bind_workspace_path(
            &mut command,
            "XDG_CACHE_HOME",
            pc::WorkspaceLayout::XDG_CACHE_ROOT,
        );
        pc::materialize_process_command(
            &self.base_env,
            command,
            self.lifecycle.secret_broker.as_ref(),
        )
        .await
    }
}

#[cfg(test)]
mod configuration_home_tests {
    use super::*;

    #[test]
    fn checkpoint_sensitive_configuration_homes_have_one_open_projection() {
        /* Cause/effect table CH1: C1=ACP bridge config, C2=backend-native
         * config, C3=runtime XDG config. R1 C1+C2+C3 => one stable open-owned
         * exclusion set; omitting any row can persist a provider credential
         * cache in a product checkpoint (FMECA S5/O2/D5=50).
         */
        assert_eq!(
            runtime_configuration_homes(),
            [
                "/workspace/.acp-config",
                "/workspace/.codex",
                "/workspace/.config"
            ],
            "CH1"
        );
    }
}
