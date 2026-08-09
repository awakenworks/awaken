//! Runtime-owned process environment shared by native exec, Hand, and ACP agents.

use super::*;

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
        // Container processes share the same stable interior paths. Bind the two
        // runtime-owned variables at this common process boundary so native exec,
        // the long-lived Hand, ACP agents, and adopted environments cannot diverge.
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
        pc::materialize_process_command(
            &self.base_env,
            command,
            self.lifecycle.secret_broker.as_ref(),
        )
        .await
    }
}
