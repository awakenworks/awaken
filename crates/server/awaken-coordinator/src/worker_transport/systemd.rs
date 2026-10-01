// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This is a Linux physical-evidence adapter for the existing
// @formal reason: Worker binding and stop ports. Dispatch and Registry retain
// @formal reason: their sole durable claim and retirement authority; this file
// @formal reason: cannot acknowledge a Run or Session cleanup.
//! Independent Linux user-service evidence for one exact Worker incarnation.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use awaken_dispatch::{
    ApplicationError, AttemptExecutionBinding, AttemptStopStatus, ClosedAttemptExecution,
    ClosedWorkerExecution, NativeAttemptFootprint, NativeAttemptTermination,
    WorkerExecutionBindingSource, WorkerExecutionStopSource, WorkerIdentity,
    WorkerProcessStopStatus,
};
use awaken_worker_contract::is_local_process_incarnation as incarnation;
use serde::{Deserialize, Serialize};

const UNIT_PREFIX: &str = "awaken-agent-worker-";
const UNIT_SUFFIX: &str = ".service";
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

fn trusted_operator_executable(name: &str) -> Option<PathBuf> {
    awaken_local_process::resolve_operator_executable(
        name,
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

pub(super) fn run_executable() -> Option<PathBuf> {
    trusted_operator_executable("systemd-run")
}

pub(super) fn control_executable() -> Option<PathBuf> {
    trusted_operator_executable("systemctl")
}

pub(super) fn env_executable() -> Option<PathBuf> {
    trusted_operator_executable("env")
}

const LOCAL_WORKER_ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

fn launch_assignment(key: &str, value: &std::ffi::OsStr) -> Result<std::ffi::OsString, String> {
    if value
        .to_string_lossy()
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err("local Worker launch environment contains a newline".into());
    }
    let mut assignment = std::ffi::OsString::from(key);
    assignment.push("=");
    assignment.push(value);
    Ok(assignment)
}

/// Process-placement inputs only. The Worker still receives its model,
/// credential, sandbox and endpoint policy through the existing deployment
/// document and signed control plane, never an ambient provider key.
pub(super) fn launch_environment() -> Result<Vec<std::ffi::OsString>, String> {
    let mut assignments = Vec::new();
    for key in LOCAL_WORKER_ENVIRONMENT_KEYS {
        if let Some(value) = std::env::var_os(key) {
            assignments.push(launch_assignment(key, &value)?);
        }
    }
    Ok(assignments)
}

pub(super) async fn available() -> bool {
    let Some(systemctl) = control_executable() else {
        return false;
    };
    if run_executable().is_none() || env_executable().is_none() {
        return false;
    }
    systemctl_command(&systemctl, &["show-environment"])
        .await
        .is_ok()
}

async fn systemctl_command(
    systemctl: &Path,
    arguments: &[&str],
) -> Result<String, ApplicationError> {
    let mut command = tokio::process::Command::new(systemctl);
    command.args(["--user", "--no-pager"]).args(arguments);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(QUERY_TIMEOUT, command.output())
        .await
        .map_err(|_| ApplicationError::unavailable("systemd Worker control timed out"))?
        .map_err(|_| ApplicationError::unavailable("systemd Worker control unavailable"))?;
    if !output.status.success() {
        return Err(ApplicationError::unavailable(
            "systemd Worker control rejected the exact unit operation",
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ApplicationError::unavailable("systemd returned non-UTF8 unit evidence"))
}

/// Placement observes liveness through the same exact unit parser as physical
/// binding, but receives no exit receipt or Dispatch retirement authority.
pub(super) async fn unit_running(incarnation_id: &str) -> Result<bool, String> {
    let unit = unit_name_for_incarnation(incarnation_id)?;
    let systemctl = control_executable().ok_or("trusted systemctl is unavailable")?;
    let output = systemctl_command(
        &systemctl,
        &[
            "show",
            &unit,
            "--property=Id,LoadState,InvocationID,ActiveState,SubState,MainPID,ControlGroup,KillMode,Restart,RemainAfterExit",
        ],
    )
    .await
    .map_err(|error| error.to_string())?;
    let observation = UnitObservation::parse(&output).map_err(|error| error.to_string())?;
    if observation.retired_after_receipt(&unit) {
        return Ok(false);
    }
    if !observation.exact_policy(&unit) {
        return Err("local Worker unit policy changed".into());
    }
    if observation.bind(&unit, &current_boot_id()?).is_some() {
        return Ok(true);
    }
    if observation.value("MainPID") == "0"
        && observation.value("ControlGroup").is_empty()
        && matches!(
            (
                observation.value("ActiveState"),
                observation.value("SubState")
            ),
            ("failed", "failed") | ("active", "exited") | ("inactive", "dead")
        )
    {
        return Ok(false);
    }
    Err("local Worker unit state is indeterminate".into())
}

pub(super) async fn request_drain(incarnation_id: &str) -> Result<(), String> {
    if !unit_running(incarnation_id).await? {
        return Ok(());
    }
    let unit = unit_name_for_incarnation(incarnation_id)?;
    let systemctl = control_executable().ok_or("trusted systemctl is unavailable")?;
    systemctl_command(
        &systemctl,
        &["kill", "--kill-who=main", "--signal=TERM", &unit],
    )
    .await
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessBinding {
    unit: String,
    invocation_id: String,
    boot_id: String,
    control_group: String,
    main_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExitReceipt {
    binding: ProcessBinding,
    observed_state: String,
}

#[derive(Debug)]
struct UnitObservation {
    values: BTreeMap<String, String>,
}

impl UnitObservation {
    fn parse(output: &str) -> Result<Self, ApplicationError> {
        let mut values = BTreeMap::new();
        for line in output.lines() {
            let (key, value) = line.split_once('=').ok_or_else(|| {
                ApplicationError::unavailable("systemd returned an invalid Worker unit property")
            })?;
            if values.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(ApplicationError::unavailable(
                    "systemd returned a duplicate Worker unit property",
                ));
            }
        }
        Ok(Self { values })
    }

    fn value(&self, key: &str) -> &str {
        self.values.get(key).map(String::as_str).unwrap_or("")
    }

    fn exact_policy(&self, unit: &str) -> bool {
        self.value("Id") == unit
            && self.value("LoadState") == "loaded"
            && self.value("KillMode") == "control-group"
            && self.value("Restart") == "no"
            && self.value("RemainAfterExit") == "yes"
    }

    fn bind(&self, unit: &str, boot_id: &str) -> Option<ProcessBinding> {
        let main_pid = self.value("MainPID").parse::<u32>().ok()?;
        let invocation_id = self.value("InvocationID");
        let control_group = self.value("ControlGroup");
        (self.exact_policy(unit)
            && self.value("ActiveState") == "active"
            && self.value("SubState") == "running"
            && main_pid != 0
            && incarnation(invocation_id)
            && control_group.ends_with(&format!("/{unit}")))
        .then(|| ProcessBinding {
            unit: unit.to_owned(),
            invocation_id: invocation_id.to_owned(),
            boot_id: boot_id.to_owned(),
            control_group: control_group.to_owned(),
            main_pid,
        })
    }

    fn exact_exit(&self, binding: &ProcessBinding) -> bool {
        self.exact_policy(&binding.unit)
            && self.value("InvocationID") == binding.invocation_id
            && self.value("ControlGroup").is_empty()
            && self.value("MainPID") == "0"
            && matches!(
                (self.value("ActiveState"), self.value("SubState")),
                ("failed", "failed") | ("active", "exited")
            )
    }

    fn retired_after_receipt(&self, unit: &str) -> bool {
        self.value("Id") == unit
            && self.value("LoadState") == "not-found"
            && self.value("MainPID") == "0"
            && self.value("ControlGroup").is_empty()
            && matches!(self.value("ActiveState"), "inactive")
    }
}

fn unit_name(worker: &WorkerIdentity) -> Result<String, ApplicationError> {
    unit_name_for_incarnation(&worker.incarnation_id).map_err(ApplicationError::invalid)
}

pub(super) fn unit_name_for_incarnation(incarnation_id: &str) -> Result<String, String> {
    if !incarnation(incarnation_id) {
        return Err("systemd Worker incarnation must be exactly 32 lowercase hex digits".into());
    }
    Ok(format!("{UNIT_PREFIX}{incarnation_id}{UNIT_SUFFIX}"))
}

fn boot_replacement_is_closed(
    binding: &ProcessBinding,
    current_boot_id: &str,
    observed: &UnitObservation,
) -> bool {
    binding.boot_id != current_boot_id && observed.retired_after_receipt(&binding.unit)
}

pub(super) struct SystemdWorkerEvidence {
    provider_id: String,
    systemctl: PathBuf,
    evidence_dir: PathBuf,
    boot_id: String,
}

fn current_boot_id() -> Result<String, String> {
    let text = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read local kernel boot identity: {error}"))?;
    uuid::Uuid::parse_str(text.trim())
        .map(|boot| boot.to_string())
        .map_err(|error| format!("invalid local kernel boot identity: {error}"))
}

impl SystemdWorkerEvidence {
    pub(super) fn new(provider_id: &str, evidence_dir: &Path) -> Result<Self, String> {
        if provider_id.trim().is_empty() || provider_id.trim() != provider_id {
            return Err("systemd Worker provider_id must be exact and nonempty".into());
        }
        if !evidence_dir.is_absolute() {
            return Err("systemd Worker evidence directory must be absolute".into());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(evidence_dir)
            .map_err(|error| format!("create systemd Worker evidence directory: {error}"))?;
        let metadata = fs::symlink_metadata(evidence_dir)
            .map_err(|error| format!("inspect systemd Worker evidence directory: {error}"))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(
                "systemd Worker evidence directory must be a private real directory".into(),
            );
        }
        awaken_fs_durability::sync_directory(evidence_dir)
            .map_err(|error| format!("sync systemd Worker evidence directory: {error}"))?;
        let systemctl = trusted_operator_executable("systemctl")
            .ok_or_else(|| "systemd Worker transport requires a trusted systemctl".to_owned())?;
        Ok(Self {
            provider_id: provider_id.to_owned(),
            systemctl,
            evidence_dir: evidence_dir.to_owned(),
            boot_id: current_boot_id()?,
        })
    }

    async fn systemctl(&self, arguments: &[&str]) -> Result<String, ApplicationError> {
        systemctl_command(&self.systemctl, arguments).await
    }

    async fn show(&self, unit: &str) -> Result<UnitObservation, ApplicationError> {
        let output = self
            .systemctl(&[
                "show",
                unit,
                "--property=Id,LoadState,InvocationID,ActiveState,SubState,MainPID,ControlGroup,KillMode,Restart,RemainAfterExit",
            ])
            .await?;
        UnitObservation::parse(&output)
    }

    fn binding_for_execution(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<ProcessBinding, ApplicationError> {
        let binding = execution.binding();
        let process: ProcessBinding = serde_json::from_str(binding.execution_ref())
            .map_err(|_| ApplicationError::invalid("invalid persisted systemd Worker binding"))?;
        if binding.provider_id() != self.provider_id
            || process.unit != unit_name(binding.worker())?
            || !incarnation(&process.invocation_id)
            || uuid::Uuid::parse_str(&process.boot_id).is_err()
            || process.main_pid == 0
            || !process
                .control_group
                .ends_with(&format!("/{}", process.unit))
        {
            return Err(ApplicationError::invalid(
                "closed Worker binding does not belong to this systemd execution domain",
            ));
        }
        Ok(process)
    }

    fn receipt_path(&self, binding: &ProcessBinding) -> PathBuf {
        self.evidence_dir.join(format!("{}.json", binding.unit))
    }

    fn read_receipt(
        &self,
        binding: &ProcessBinding,
    ) -> Result<Option<ExitReceipt>, ApplicationError> {
        let path = self.receipt_path(binding);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(ApplicationError::unavailable(
                    "systemd exit evidence cannot be inspected",
                ));
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ApplicationError::unavailable(
                "systemd exit evidence is not a regular file",
            ));
        }
        let bytes = fs::read(path)
            .map_err(|_| ApplicationError::unavailable("systemd exit evidence cannot be read"))?;
        let receipt: ExitReceipt = serde_json::from_slice(&bytes)
            .map_err(|_| ApplicationError::unavailable("systemd exit evidence is invalid"))?;
        if receipt.binding != *binding
            || !matches!(
                receipt.observed_state.as_str(),
                "failed:failed" | "active:exited" | "boot-replaced"
            )
        {
            return Err(ApplicationError::unavailable(
                "systemd exit evidence names another process or outcome",
            ));
        }
        // A response-lost writer may have linked its already-synced file but
        // crashed before syncing the directory. Make that link durable before
        // allowing the Registry to store a provider exit observation.
        awaken_fs_durability::sync_directory(&self.evidence_dir).map_err(|_| {
            ApplicationError::unavailable("systemd exit evidence directory cannot be synced")
        })?;
        Ok(Some(receipt))
    }

    fn write_receipt(&self, receipt: &ExitReceipt) -> Result<(), ApplicationError> {
        if self.read_receipt(&receipt.binding)?.is_some() {
            // Two positive observations of the same process may see failed,
            // then boot-replaced. The binding, not that later display state,
            // identifies the one physical exit receipt.
            return Ok(());
        }
        let path = self.receipt_path(&receipt.binding);
        let temporary = self.evidence_dir.join(format!(
            ".{}.{}.tmp",
            receipt.binding.unit,
            uuid::Uuid::now_v7()
        ));
        let bytes = serde_json::to_vec(receipt)
            .map_err(|_| ApplicationError::internal("systemd exit evidence encoding failed"))?;
        let write = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::hard_link(&temporary, &path)?;
            awaken_fs_durability::sync_directory(&self.evidence_dir)?;
            fs::remove_file(&temporary)?;
            awaken_fs_durability::sync_directory(&self.evidence_dir)
        })();
        if let Err(error) = write {
            let _ = fs::remove_file(&temporary);
            if error.kind() == io::ErrorKind::AlreadyExists
                && self.read_receipt(&receipt.binding)?.is_some()
            {
                return Ok(());
            }
            return Err(ApplicationError::unavailable(format!(
                "systemd exit evidence could not be persisted: {error}"
            )));
        }
        Ok(())
    }

    async fn retire_unit(
        &self,
        binding: &ProcessBinding,
    ) -> Result<Option<String>, ApplicationError> {
        self.systemctl(&["stop", &binding.unit]).await?;
        // A failed transient unit remains loaded (and manually restartable)
        // after StopUnit. The durable exact exit receipt now owns the evidence,
        // so release systemd's failed-unit history before permitting replay.
        let _ = self.systemctl(&["reset-failed", &binding.unit]).await;
        for _ in 0..100 {
            if self
                .show(&binding.unit)
                .await?
                .retired_after_receipt(&binding.unit)
            {
                return Ok(Some(format!(
                    "systemd-user:{}:{}",
                    binding.unit, binding.invocation_id
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(None)
    }

    async fn stop_binding(
        &self,
        binding: &ProcessBinding,
    ) -> Result<Option<String>, ApplicationError> {
        if binding.boot_id != self.boot_id {
            let observed = self.show(&binding.unit).await?;
            if !boot_replacement_is_closed(binding, &self.boot_id, &observed) {
                // A newly loaded same-name unit is foreign; never stop it.
                return Ok(None);
            }
            self.write_receipt(&ExitReceipt {
                binding: binding.clone(),
                observed_state: "boot-replaced".into(),
            })?;
            return Ok(Some(format!(
                "systemd-user:{}:{}:boot-replaced",
                binding.unit, binding.invocation_id
            )));
        }
        if self.read_receipt(binding)?.is_some() {
            let observed = self.show(&binding.unit).await?;
            if observed.exact_exit(binding) {
                // A crash after the durable receipt but before StopUnit keeps
                // the old invocation visible. Finish the same retirement;
                // never treat the receipt alone as permission to re-enter.
                return self.retire_unit(binding).await;
            }
            return Ok(observed
                .retired_after_receipt(&binding.unit)
                .then(|| format!("systemd-user:{}:{}", binding.unit, binding.invocation_id)));
        }
        let mut observed = self.show(&binding.unit).await?;
        if observed.value("InvocationID") != binding.invocation_id
            || !observed.exact_policy(&binding.unit)
        {
            return Ok(None);
        }
        if !observed.value("ControlGroup").is_empty() {
            self.systemctl(&["kill", "--kill-who=all", "--signal=SIGKILL", &binding.unit])
                .await?;
        }
        for _ in 0..100 {
            observed = self.show(&binding.unit).await?;
            if observed.exact_exit(binding) {
                let receipt = ExitReceipt {
                    binding: binding.clone(),
                    observed_state: format!(
                        "{}:{}",
                        observed.value("ActiveState"),
                        observed.value("SubState")
                    ),
                };
                self.write_receipt(&receipt)?;
                return self.retire_unit(binding).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(None)
    }
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for SystemdWorkerEvidence {
    async fn binding_for(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<AttemptExecutionBinding, ApplicationError> {
        let unit = unit_name(worker)?;
        let observation = self.show(&unit).await?;
        let process = observation.bind(&unit, &self.boot_id).ok_or_else(|| {
            ApplicationError::unavailable("exact systemd Worker process is unavailable")
        })?;
        let reference = serde_json::to_string(&process)
            .map_err(|_| ApplicationError::internal("systemd Worker binding encoding failed"))?;
        AttemptExecutionBinding::new(worker.clone(), &self.provider_id, reference)
            .map_err(|error| ApplicationError::invalid(error.to_string()))
    }
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for SystemdWorkerEvidence {
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let binding = self.binding_for_execution(execution)?;
        match self.stop_binding(&binding).await? {
            Some(receipt) => execution.observed_exit(receipt),
            None => Ok(WorkerProcessStopStatus::Unknown),
        }
    }

    async fn release_closed_worker_evidence(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<(), ApplicationError> {
        let binding = self.binding_for_execution(execution)?;
        if self.read_receipt(&binding)?.is_none() {
            return awaken_fs_durability::sync_directory(&self.evidence_dir).map_err(|_| {
                ApplicationError::unavailable("systemd exit evidence release cannot be verified")
            });
        }
        fs::remove_file(self.receipt_path(&binding)).map_err(|_| {
            ApplicationError::unavailable("systemd exit evidence could not be released")
        })?;
        awaken_fs_durability::sync_directory(&self.evidence_dir).map_err(|_| {
            ApplicationError::unavailable("systemd exit evidence release was not durable")
        })
    }

    async fn close_attempt_execution(
        &self,
        execution: &ClosedAttemptExecution,
        recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        let footprint = NativeAttemptFootprint::decode(recovery)
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        if !footprint.model_only_coverage() {
            return Ok(AttemptStopStatus::Unknown);
        }
        let evidence = format!(
            "{};thread-commit:{}",
            execution.local_evidence_ref(),
            recovery.store_cursor
        );
        match footprint.classify(&[]) {
            NativeAttemptTermination::AllExecutionStopped => {
                execution.observed_all_execution_stopped(evidence)
            }
            // A local process exit does not establish the outcome of an
            // outstanding provider request. Keep response-only inference
            // Unknown until its own provider supplies terminal evidence.
            NativeAttemptTermination::ControlledExecutionClosed => Ok(AttemptStopStatus::Unknown),
            NativeAttemptTermination::Unknown => Ok(AttemptStopStatus::Unknown),
        }
    }
}

#[cfg(test)]
mod tests;
