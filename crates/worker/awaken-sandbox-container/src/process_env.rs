//! Runtime-owned process environment shared by native exec, Hand, and ACP agents.

use super::*;

fn workspace_scoped_path(value: &str) -> bool {
    let path = std::path::Path::new(value);
    path.is_absolute()
        && path.starts_with("/workspace")
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

/// Keep the caller's last explicit workspace-scoped path, otherwise install
/// the runtime default. All duplicates are collapsed so backend argv ordering
/// cannot silently change the selected process home.
fn bind_workspace_path(command: &mut pc::Command, name: &str, default: &str) {
    let selected = command.env.iter().rev().find_map(|var| {
        if var.name != name {
            return None;
        }
        let pc::EnvValue::Inline { value } = &var.value else {
            return None;
        };
        workspace_scoped_path(value).then(|| value.clone())
    });
    command.env.retain(|var| var.name != name);
    command.env.push(pc::EnvVar {
        name: name.into(),
        value: pc::EnvValue::Inline {
            value: selected.unwrap_or_else(|| default.into()),
        },
        visibility: pc::EnvVisibility::Process,
    });
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
                    value: "/workspace".into(),
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
        bind_workspace_path(&mut command, "HOME", "/workspace");
        bind_workspace_path(&mut command, "XDG_CONFIG_HOME", "/workspace/.config");
        bind_workspace_path(&mut command, "XDG_CACHE_HOME", "/workspace/.cache");
        pc::materialize_process_command(
            &self.base_env,
            command,
            self.lifecycle.secret_broker.as_ref(),
        )
        .await
    }
}
