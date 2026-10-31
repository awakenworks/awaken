// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: The existing Workdir provider owns final cwd and environment realization
// @formal reason: before spawn; materialized credentials enter only the child command after
// @formal reason: contract admission. This is a product-boundary classification, not proof.
//! Workdir child command preparation shared by process and Agent-channel spawn.

use super::*;
use tokio::process::Command as TokioCommand;

pub(super) struct PreparedCommand {
    pub(super) command: pc::MaterializedCommand,
    pub(super) cwd: PathBuf,
}

impl PreparedCommand {
    pub(super) fn into_tokio(self) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.command.argv[0]);
        cmd.args(&self.command.argv[1..])
            .current_dir(self.cwd)
            .env_clear();
        for var in &self.command.env {
            cmd.env(&var.name, var.value.expose());
        }
        #[cfg(windows)]
        for name in crate::workdir_shell::WINDOWS_OS_LOCATORS {
            if !self
                .command
                .env
                .iter()
                .any(|var| var.name.eq_ignore_ascii_case(name))
                && let Some(value) = std::env::var_os(name)
            {
                cmd.env(name, value);
            }
        }
        cmd
    }
}

impl LocalSandbox {
    /// Build the child command (program, args, jailed cwd, reserved + declared env),
    /// leaving stdio for the caller to configure. Shared by `spawn`/`spawn_agent`.
    pub(super) async fn build_command(
        &self,
        command: pc::Command,
    ) -> Result<PreparedCommand, pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let broker = self.secret_broker.read().clone();
        let mut command =
            pc::materialize_process_command(&self.base_env, command, broker.as_ref()).await?;
        if command.argv.is_empty() {
            return Err(pc::SandboxError::invalid("command argv is empty"));
        }
        let cwd_logical = if command.cwd.is_empty() {
            "/"
        } else {
            &command.cwd
        };
        let host_cwd = self
            .root
            .resolve(cwd_logical)
            .map_err(pc::SandboxError::from)?;
        awaken_fs_durability::create_relative_directory_all(
            self.root.root(),
            root_identity,
            host_cwd.strip_prefix(self.root.root()).map_err(|_| {
                pc::SandboxError::invalid("command cwd escaped its exact sandbox root")
            })?,
        )
        .map_err(|error| pc::SandboxError::backend("build_command", error))?;
        let host_outputs = self.host_outputs()?;
        awaken_fs_durability::create_relative_directory_all(
            self.root.root(),
            root_identity,
            host_outputs.strip_prefix(self.root.root()).map_err(|_| {
                pc::SandboxError::invalid("outputs path escaped its exact sandbox root")
            })?,
        )
        .map_err(|error| pc::SandboxError::backend("build_command", error))?;

        // Reserved, runtime-owned env: where the agent writes/works (the local tier
        // has no path fidelity, so a process finds the outputs dir via this var).
        let paths = crate::RuntimePathEnv::new(
            host_cwd.to_string_lossy().into_owned(),
            host_outputs.to_string_lossy().into_owned(),
        );
        for (name, value) in paths.entries() {
            if !command.env.iter().any(|var| var.name == name) {
                command.env.push(pc::MaterializedEnvVar {
                    name: name.into(),
                    value: pc::MaterializedEnvValue::Inline(value.into()),
                });
            }
        }
        Ok(PreparedCommand {
            command,
            cwd: host_cwd,
        })
    }
}
