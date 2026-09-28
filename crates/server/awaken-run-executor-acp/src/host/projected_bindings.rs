//! Read-only host-login binding projection. The external projector owns intent;
//! this adapter owns only bounded reads and execution-time liveness evidence.

use super::*;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::io::AsyncReadExt;

const MAX_BINDING_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionRecheckDecision {
    ReturnObservation,
    Reject,
}

/// The single post-probe admission decision used by both batch observation and
/// exact-reference revalidation. Probe success is deliberately not an input:
/// its original result is returned only when the complete projection is still
/// byte-semantically identical.
const fn projection_recheck_decision(exact_projection: bool) -> ProjectionRecheckDecision {
    if exact_projection {
        ProjectionRecheckDecision::ReturnObservation
    } else {
        ProjectionRecheckDecision::Reject
    }
}

#[cfg(kani)]
/// @obligation worker_credential.projected_binding_use_rechecks_after_probe
#[kani::proof]
fn projected_binding_use_rechecks_the_exact_projection_after_probe() {
    // Cause C1: the post-probe projection exactly matches the pre-probe
    // projection. Effects E1/E2: return the original observation outcome or
    // reject it as unavailable. The two rules are exhaustive.
    //
    // | Rule | C1 | Effect |
    // |---|---|---|
    // | R1 | yes | E1 return observation |
    // | R2 | no  | E2 reject |
    let exact_projection: bool = kani::any();
    let decision = projection_recheck_decision(exact_projection);
    assert_eq!(
        decision == ProjectionRecheckDecision::ReturnObservation,
        exact_projection
    );
    assert_eq!(
        decision == ProjectionRecheckDecision::Reject,
        !exact_projection
    );
}

/// Secret-free metadata, not a credential value or a login-file location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpLocalCredentialBinding {
    #[serde(deserialize_with = "deserialize_reference")]
    pub credential: CredentialRef,
    pub cli_id: String,
    pub enabled: bool,
}

fn deserialize_reference<'de, D: Deserializer<'de>>(d: D) -> Result<CredentialRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictReference {
        id: String,
        revision: u64,
    }
    let value = StrictReference::deserialize(d)?;
    Ok(CredentialRef {
        id: value.id,
        revision: value.revision,
    })
}

impl AcpLocalCredentialBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.credential.id.trim().is_empty() || self.credential.revision == 0 {
            return Err(
                "WorkerLocal credential requires a nonblank id and positive revision".into(),
            );
        }
        if acp_cli(&self.cli_id).is_none() {
            return Err("WorkerLocal binding names an unknown ACP CLI".into());
        }
        Ok(())
    }

    pub(super) fn to_local_binding(&self) -> Result<LocalBinding, String> {
        self.validate()?;
        Ok(LocalBinding {
            credential: self.credential.clone(),
            cli: acp_cli(&self.cli_id).expect("validated catalog id"),
            enabled: self.enabled,
        })
    }
}

/// A complete external projection, atomically replaced by its trusted publisher.
/// No Control repository, secret reader, writer, polling task or second map.
/// Missing files revoke all bindings; malformed/unreadable files fail closed.
/// Arbitrary rollback by the trusted publisher and the final check-to-launch
/// interval are not fenced by this local adapter.
pub struct ProjectedAcpCredentials {
    resolver: Arc<AcpLocalCredentialResolver>,
    path: PathBuf,
    selected_cli_ids: BTreeSet<String>,
    refresh: tokio::sync::Mutex<()>,
}

impl ProjectedAcpCredentials {
    pub fn new(
        resolver: Arc<AcpLocalCredentialResolver>,
        path: PathBuf,
        selected_cli_ids: BTreeSet<String>,
    ) -> Result<Self, String> {
        if path.as_os_str().is_empty()
            || selected_cli_ids.is_empty()
            || selected_cli_ids.iter().any(|id| acp_cli(id).is_none())
        {
            return Err(
                "projected host bindings require a path and known selected ACP CLIs".into(),
            );
        }
        Ok(Self {
            resolver,
            path,
            selected_cli_ids,
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    async fn read(&self) -> Result<Vec<AcpLocalCredentialBinding>, CredentialMaterialError> {
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        // Refuse FIFOs/devices after opening the same handle without blocking
        // on a FIFO open. Symlinks remain usable for atomically projected volumes.
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = match options.open(&self.path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(CredentialMaterialError::Unavailable),
        };
        let metadata = file
            .metadata()
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > MAX_BINDING_BYTES {
            return Err(CredentialMaterialError::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BINDING_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if bytes.len() as u64 > MAX_BINDING_BYTES {
            return Err(CredentialMaterialError::Invalid);
        }
        let bindings: Vec<AcpLocalCredentialBinding> =
            serde_json::from_slice(&bytes).map_err(|_| CredentialMaterialError::Invalid)?;
        let mut ids = BTreeSet::new();
        for binding in &bindings {
            binding
                .validate()
                .map_err(|_| CredentialMaterialError::Invalid)?;
            if !self.selected_cli_ids.contains(&binding.cli_id)
                || !ids.insert(&binding.credential.id)
            {
                return Err(CredentialMaterialError::Invalid);
            }
        }
        Ok(bindings)
    }

    async fn reload(&self) -> Result<Vec<AcpLocalCredentialBinding>, CredentialMaterialError> {
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), self.read()).await;
        match result {
            Ok(Ok(bindings)) => {
                let next = bindings
                    .iter()
                    .map(|binding| Ok((binding.credential.id.clone(), binding.to_local_binding()?)))
                    .collect::<Result<BTreeMap<_, _>, String>>()
                    .map_err(|_| CredentialMaterialError::Invalid)?;
                *self
                    .resolver
                    .bindings
                    .lock()
                    .expect("ACP bindings poisoned") = next;
                Ok(bindings)
            }
            failure => {
                // Never leave a usable prior binding behind on a hard read failure.
                self.resolver
                    .bindings
                    .lock()
                    .expect("ACP bindings poisoned")
                    .clear();
                Err(match failure {
                    Ok(Err(error)) => error,
                    _ => CredentialMaterialError::Unavailable,
                })
            }
        }
    }

    /// Validate and install the initial projection without probing a login.
    pub async fn refresh(&self) -> Result<(), CredentialMaterialError> {
        let _guard = self.refresh.lock().await;
        self.reload().await.map(|_| ())
    }
}

#[async_trait]
impl CredentialObservationSource for ProjectedAcpCredentials {
    async fn credential_observations(
        &self,
    ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
        let _guard = self.refresh.lock().await;
        let before = self.reload().await?;
        let observations = self.resolver.credential_observations().await?;
        let after = self.reload().await?;
        // Ordering in the JSON array is not authority. Compare exact identities.
        let normalize = |rows: Vec<AcpLocalCredentialBinding>| {
            rows.into_iter()
                .map(|row| (row.credential.id.clone(), row))
                .collect::<BTreeMap<_, _>>()
        };
        match projection_recheck_decision(normalize(before) == normalize(after)) {
            ProjectionRecheckDecision::ReturnObservation => Ok(observations),
            ProjectionRecheckDecision::Reject => Err(CredentialMaterialError::Unavailable),
        }
    }
}

#[async_trait]
impl WorkerLocalReferenceRevalidator for ProjectedAcpCredentials {
    async fn revalidate_worker_reference(
        &self,
        credential: &CredentialRef,
    ) -> Result<CredentialObservation, CredentialMaterialError> {
        let _guard = self.refresh.lock().await;
        let before = self
            .reload()
            .await?
            .into_iter()
            .find(|row| &row.credential == credential)
            .ok_or(CredentialMaterialError::Unavailable)?;
        let observed = self.resolver.revalidate_worker_reference(credential).await;
        let after = self
            .reload()
            .await?
            .into_iter()
            .find(|row| &row.credential == credential);
        match projection_recheck_decision(after.as_ref() == Some(&before)) {
            ProjectionRecheckDecision::ReturnObservation => observed,
            ProjectionRecheckDecision::Reject => Err(CredentialMaterialError::Unavailable),
        }
    }
}

#[async_trait]
impl AcpCapabilityObservationSource for ProjectedAcpCredentials {
    async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
        let _guard = self.refresh.lock().await;
        self.resolver.capability_observations().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn post_probe_projection_decision_is_exact_and_fail_closed() {
        // Causes: C1 the complete projection is unchanged; C2 any coordinate
        // changed. Effects: E1 return the probe outcome; E2 reject it. These
        // two rows cover the closed Boolean quotient used by both callers.
        //
        // | Rule | exact projection | decision |
        // |---|---|---|
        // | R1 | yes | return observation |
        // | R2 | no  | reject |
        assert_eq!(
            projection_recheck_decision(true),
            ProjectionRecheckDecision::ReturnObservation,
            "R1/E1"
        );
        assert_eq!(
            projection_recheck_decision(false),
            ProjectionRecheckDecision::Reject,
            "R2/E2"
        );
    }

    struct Discovery {
        calls: AtomicUsize,
        pause: AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        state: CredentialObservationState,
        reply_cli: Option<&'static str>,
    }
    impl Discovery {
        fn new(state: CredentialObservationState) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                pause: AtomicBool::new(false),
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                state,
                reply_cli: None,
            }
        }
    }
    #[async_trait]
    impl AcpDiscovery for Discovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.pause.swap(false, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            AcpHostObservation {
                cli_id: self.reply_cli.unwrap_or(cli.id).into(),
                display_name: cli.id.into(),
                detection: AcpDetectionState::Detected,
                version: Some("fixture".into()),
                credential_state: Some(self.state),
                reason_code: None,
                capability_state: None,
                capability_fingerprint: None,
                capability_reason_code: None,
            }
        }
    }
    fn row(cli_id: &str, revision: u64, enabled: bool) -> AcpLocalCredentialBinding {
        AcpLocalCredentialBinding {
            credential: CredentialRef {
                id: "ref-a".into(),
                revision,
            },
            cli_id: cli_id.into(),
            enabled,
        }
    }
    fn write(path: &Path, rows: &[AcpLocalCredentialBinding]) {
        // Same-filesystem complete replacement exercises the supported projector
        // contract on Unix AND Windows, including readers holding the old handle.
        let mut next = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
        std::io::Write::write_all(&mut next, &serde_json::to_vec(rows).unwrap()).unwrap();
        next.persist(path).unwrap();
    }
    fn projected(path: &Path, discovery: Arc<Discovery>) -> Arc<ProjectedAcpCredentials> {
        Arc::new(
            ProjectedAcpCredentials::new(
                Arc::new(AcpLocalCredentialResolver::from_bindings(discovery, []).unwrap()),
                path.into(),
                ["codex".into(), "claude".into()].into_iter().collect(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn discovery_catalog_walk_scopes_each_reply_before_startup_projection() {
        // Causes: C1 each catalog request; C2 adapter replies with a fixed known
        // or unknown backend. R1 exact reply retains its evidence; R2 mismatch
        // becomes a failed observation for the requested backend with no borrowed
        // version/login/capability fields. The actual default discover_all method
        // also serves startup and diagnostics, beyond resolver cache/fallback.
        for reply_cli in ["codex", "unknown"] {
            let mut discovery = Discovery::new(CredentialObservationState::Available);
            discovery.reply_cli = Some(reply_cli);
            let observations = discovery.discover_all().await;
            assert_eq!(observations.len(), crate::known_acp_clis().len());
            for (cli, observed) in crate::known_acp_clis().iter().zip(observations) {
                assert_eq!(observed.cli_id, cli.id, "DS1 catalog request identity");
                if cli.id == reply_cli {
                    assert!(observed.detected(), "DS1 exact backend remains useful");
                } else {
                    assert_eq!(observed.detection, AcpDetectionState::ProbeFailed, "DS2");
                    assert_eq!(
                        observed.reason_code.as_deref(),
                        Some("acp_discovery_backend_mismatch")
                    );
                    assert!(
                        observed.version.is_none()
                            && observed.credential_state.is_none()
                            && observed.capability_state.is_none()
                            && observed.capability_fingerprint.is_none(),
                        "DS2 no foreign evidence"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn projection_lifecycle_and_exact_login_decision_table() {
        // Causes: selected CLI, exact revision, enabled intent, each host state,
        // complete replacement/removal. R1 both Codex/Claude x all six login
        // states preserve exact observations and only Available allows use.
        // R2 disabled/absent/mismatched refs never probe; R3 rotation replaces
        // rather than extends the map; R4 file disappearance and [] both revoke.
        for cli in ["codex", "claude"] {
            for state in [
                CredentialObservationState::Available,
                CredentialObservationState::LoginRequired,
                CredentialObservationState::Expired,
                CredentialObservationState::Invalid,
                CredentialObservationState::Disabled,
                CredentialObservationState::ProbeFailed,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("路径 bindings.json");
                let discovery = Arc::new(Discovery::new(state));
                let projected = projected(&path, discovery.clone());
                let binding = row(cli, 1, true);
                write(&path, std::slice::from_ref(&binding));
                let observed = projected.credential_observations().await.unwrap();
                assert_eq!(observed.len(), 1);
                assert_eq!(observed.first().unwrap().state, state);
                assert_eq!(observed.first().unwrap().credential, binding.credential);
                assert_eq!(
                    projected
                        .revalidate_worker_reference(&binding.credential)
                        .await
                        .is_ok(),
                    state == CredentialObservationState::Available,
                    "R1 {cli} {state:?}"
                );
                assert_eq!(discovery.calls.load(Ordering::SeqCst), 2);
                let rotated = row(cli, 2, false);
                write(&path, std::slice::from_ref(&rotated));
                assert!(
                    projected
                        .revalidate_worker_reference(&binding.credential)
                        .await
                        .is_err(),
                    "R3"
                );
                assert_eq!(
                    projected
                        .revalidate_worker_reference(&rotated.credential)
                        .await,
                    Err(CredentialMaterialError::Disabled),
                    "R2"
                );
                let disabled = projected.credential_observations().await.unwrap();
                assert_eq!(disabled.first().unwrap().credential, rotated.credential);
                assert_eq!(
                    disabled.first().unwrap().state,
                    CredentialObservationState::Disabled
                );
                assert_eq!(
                    discovery.calls.load(Ordering::SeqCst),
                    2,
                    "R2 zero new probe"
                );
                for missing in [false, true] {
                    if missing {
                        std::fs::remove_file(&path).unwrap();
                    } else {
                        write(&path, &[]);
                    }
                    assert!(
                        projected
                            .credential_observations()
                            .await
                            .unwrap()
                            .is_empty(),
                        "R4"
                    );
                    assert!(
                        projected
                            .revalidate_worker_reference(&rotated.credential)
                            .await
                            .is_err()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn malformed_projection_rejects_whole_batch_without_renewing_evidence() {
        // R5 valid prefix + invalid suffix (shape, identity, revision, CLI,
        // allowlist, duplicate or material-looking field) => hard error, no
        // prefix/old map left usable, zero host calls. Nested unknown keys must
        // not be silently swallowed by the shared CredentialRef deserializer.
        // R6 oversized/nonregular files => same denial; R7 valid retry recovers.
        let valid = serde_json::to_value(row("codex", 1, true)).unwrap();
        let mut cases = Vec::new();
        for (pointer, value) in [
            ("/credential/id", serde_json::json!("  ")),
            ("/credential/revision", serde_json::json!(0)),
            ("/cli_id", serde_json::json!("unknown")),
            ("/cli_id", serde_json::json!("gemini")),
            ("/enabled", serde_json::json!("true")),
        ] {
            let mut invalid = valid.clone();
            // Keep every non-identity fault independent of duplicate-id denial.
            invalid["credential"]["id"] = serde_json::json!("ref-b");
            *invalid.pointer_mut(pointer).unwrap() = value;
            cases.push(serde_json::to_vec(&vec![valid.clone(), invalid]).unwrap());
        }
        let mut extra = valid.clone();
        extra["credential"]["id"] = serde_json::json!("ref-b");
        extra
            .as_object_mut()
            .unwrap()
            .insert("token".into(), serde_json::json!("fixture-only"));
        cases.push(serde_json::to_vec(&vec![valid.clone(), extra]).unwrap());
        let mut nested = valid.clone();
        nested["credential"]["id"] = serde_json::json!("ref-b");
        nested["credential"]["token"] = serde_json::json!("fixture-only");
        cases.push(serde_json::to_vec(&vec![valid.clone(), nested]).unwrap());
        cases.extend([
            serde_json::to_vec(&vec![valid.clone(), valid]).unwrap(),
            b"[".to_vec(),
            b"{}".to_vec(),
            b"null".to_vec(),
            b"[] trailing".to_vec(),
            vec![b' '; MAX_BINDING_BYTES as usize + 1],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings.json");
        let discovery = Arc::new(Discovery::new(CredentialObservationState::Available));
        let projected = projected(&path, discovery.clone());
        for bytes in cases {
            write(&path, &[row("codex", 1, true)]);
            projected.refresh().await.unwrap();
            std::fs::write(&path, bytes).unwrap();
            assert!(projected.credential_observations().await.is_err(), "R5/R6");
            assert!(
                projected
                    .revalidate_worker_reference(&row("codex", 1, true).credential)
                    .await
                    .is_err()
            );
            assert!(projected.resolver.bindings.lock().unwrap().is_empty());
            assert_eq!(discovery.calls.load(Ordering::SeqCst), 0);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(projected.refresh().await.is_err(), "R6 directory");
        std::fs::remove_dir(&path).unwrap();
        write(&path, &[row("claude", 2, true)]);
        assert!(
            projected
                .revalidate_worker_reference(&row("claude", 2, true).credential)
                .await
                .is_ok(),
            "R7"
        );
    }

    #[tokio::test]
    async fn changes_during_probe_fence_both_heartbeat_and_execution() {
        // R8 probe awaits external process while projector disables/deletes/
        // rotates/rebinds/corrupts => old Available evidence cannot escape either
        // port. R9 unrelated row change fences whole heartbeat but not an exact
        // per-use check; R10 semantically identical replacement succeeds.
        // C4 the awaited adapter returns the requested CLI, another known CLI,
        // or an unknown CLI. R13 unchanged projections can expose only a
        // ProbeFailed heartbeat for mismatched replies; per-use never grants.
        // Cross C4 with every R8-R10 replacement to keep post-probe fencing and
        // backend scope independent, including unrelated-row per-use success.
        // Notify barriers exercise the actual await, without timing sleeps.
        for reply_cli in [None, Some("claude"), Some("unknown")] {
            for heartbeat in [false, true] {
                for change in [
                    "disable", "delete", "revision", "cli", "corrupt", "other", "same",
                ] {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("bindings.json");
                    let mut discovery = Discovery::new(CredentialObservationState::Available);
                    discovery.reply_cli = reply_cli;
                    let discovery = Arc::new(discovery);
                    discovery.pause.store(true, Ordering::SeqCst);
                    let projected = projected(&path, discovery.clone());
                    write(&path, &[row("codex", 1, true)]);
                    let operation = {
                        let projected = projected.clone();
                        tokio::spawn(async move {
                            if heartbeat {
                                projected
                                    .credential_observations()
                                    .await
                                    .map(|rows| rows.into_iter().next().unwrap().state)
                            } else {
                                projected
                                    .revalidate_worker_reference(&row("codex", 1, true).credential)
                                    .await
                                    .map(|row| row.state)
                            }
                        })
                    };
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        discovery.started.notified(),
                    )
                    .await
                    .expect("timed out waiting for discovery.started.notified()");
                    match change {
                        "disable" => write(&path, &[row("codex", 1, false)]),
                        "delete" => std::fs::remove_file(&path).unwrap(),
                        "revision" => write(&path, &[row("codex", 2, true)]),
                        "cli" => write(&path, &[row("claude", 1, true)]),
                        "corrupt" => std::fs::write(&path, b"broken").unwrap(),
                        "other" => {
                            let mut other = row("claude", 1, true);
                            other.credential.id = "ref-b".into();
                            write(&path, &[other, row("codex", 1, true)]);
                        }
                        "same" => write(&path, &[row("codex", 1, true)]),
                        _ => unreachable!(),
                    }
                    discovery.release.notify_one();
                    let result = operation.await.unwrap();
                    let unchanged = change == "same" || (!heartbeat && change == "other");
                    assert_eq!(
                        result.is_ok(),
                        unchanged && (heartbeat || reply_cli.is_none()),
                        "R8-R10/R13 {heartbeat} {change} {reply_cli:?}"
                    );
                    if let Ok(state) = result {
                        assert_eq!(
                            state,
                            if reply_cli.is_some() {
                                CredentialObservationState::ProbeFailed
                            } else {
                                CredentialObservationState::Available
                            },
                            "R13 response identity {heartbeat} {change} {reply_cli:?}"
                        );
                    }
                    assert_eq!(
                        discovery.calls.load(Ordering::SeqCst),
                        1,
                        "no duplicate login probe"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn refresh_waits_for_probe_and_cancellation_releases_the_only_gate() {
        // R11 competing reload is Pending while an exact check holds the gate;
        // after cancellation it installs the latest projection, not a saved
        // prefix. R12 failed/cancelled probe creates no permanently held lock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings.json");
        let discovery = Arc::new(Discovery::new(CredentialObservationState::Available));
        discovery.pause.store(true, Ordering::SeqCst);
        let projected = projected(&path, discovery.clone());
        write(&path, &[row("codex", 1, true)]);
        let operation = {
            let projected = projected.clone();
            tokio::spawn(async move {
                projected
                    .revalidate_worker_reference(&row("codex", 1, true).credential)
                    .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            discovery.started.notified(),
        )
        .await
        .expect("timed out waiting for discovery.started.notified()");
        write(&path, &[row("codex", 2, false)]);
        let mut refresh = std::pin::pin!(projected.refresh());
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(refresh.as_mut(), cx).is_pending(),
                "R11"
            );
            std::task::Poll::Ready(())
        })
        .await;
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
        refresh.await.unwrap();
        assert_eq!(
            projected
                .revalidate_worker_reference(&row("codex", 2, false).credential)
                .await,
            Err(CredentialMaterialError::Disabled),
            "R12"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_is_rejected_without_waiting_for_a_writer() {
        // R13 hostile/nonregular projection path => bounded rejection even if
        // no peer opens the FIFO. This specifically tests O_NONBLOCK at open.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let projected = projected(
            &path,
            Arc::new(Discovery::new(CredentialObservationState::Available)),
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), projected.refresh())
                .await
                .unwrap(),
            Err(CredentialMaterialError::Invalid)
        );
    }

    #[tokio::test]
    async fn read_error_cannot_retain_a_previous_binding_and_valid_retry_recovers() {
        // R14 ancestor changes from directory to regular file => opening the
        // projection fails (not NotFound), clears the old map and cannot probe.
        // R15 restore valid path => normal operation resumes. This portable
        // OS-error case covers fail-closed IO independently of JSON validation.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("projection");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("bindings.json");
        let discovery = Arc::new(Discovery::new(CredentialObservationState::Available));
        let projected = projected(&path, discovery.clone());
        write(&path, &[row("codex", 1, true)]);
        projected.refresh().await.unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        std::fs::write(&parent, b"not a directory").unwrap();
        assert!(
            projected
                .revalidate_worker_reference(&row("codex", 1, true).credential)
                .await
                .is_err(),
            "R14"
        );
        assert!(projected.resolver.bindings.lock().unwrap().is_empty());
        assert_eq!(discovery.calls.load(Ordering::SeqCst), 0);
        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        write(&path, &[row("codex", 2, true)]);
        assert!(
            projected
                .revalidate_worker_reference(&row("codex", 2, true).credential)
                .await
                .is_ok(),
            "R15"
        );
    }
}
