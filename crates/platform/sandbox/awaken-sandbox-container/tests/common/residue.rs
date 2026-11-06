//! Residue containment for the live-daemon suites.
//!
//! # Why this is one module
//!
//! A live-daemon test makes a daemon create objects that outlive the test
//! process, and cleanup written on the success path does not run when a test
//! panics or is killed. Two different failures follow from that, and they need
//! two different keys, which is why both live here rather than in two guards
//! that each solve half.
//!
//! *Poisoning* is the worse one. A Sandbox's physical name is a pure function
//! of its scope, so a fixed-scope test that dies meets its own leftover on the
//! next run and fails closed against a realization it holds no fence for —
//! one rare flake becoming a permanent failure only a manual `docker rm -f`
//! could clear. The key for that is the scope, swept on the way in.
//!
//! *Leaking* is the broader one. A warm pool mints its own capacity scopes
//! internally, and no test can name them, so a scope sweep cannot reach them.
//! But every managed container carries the id of the runtime incarnation that
//! created it, and that id starts with the creating process's pid. A guard
//! without a scope may use that key only when one test owns the process;
//! sweeping by pid from a scoped guard would remove another concurrently
//! running test's container in an ordinary `cargo test` binary.

use std::process::Command;

/// The label every managed container carries, used only to narrow the listing
/// before the owner check below decides what actually belongs to this test.
const MANAGED_LABEL: &str = "awaken.sandbox";

/// The label naming the runtime incarnation that created a container. Its
/// value is `{pid}-{epoch}-{sequence}`, so the creating process is a prefix.
const OWNER_LISTING: &str = "{{.ID}} {{.Label \"awaken.sandbox.owner\"}}";

/// Which daemon a suite is driving. Each test binary drives one, so the other
/// variant is unconstructed there.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum Backend {
    Docker,
    Podman,
}

impl Backend {
    const fn cli(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

/// One live suite's claim on everything it makes a daemon create.
///
/// Arming sweeps an exact scope, so a leftover from an earlier run is gone
/// before this one creates anything. Dropping a scoped guard sweeps only that
/// scope. A guard without a scope sweeps this process's containers, and is
/// reserved for suites running one test per process with unnameable scopes.
/// Both kinds of `Drop` run while a panic unwinds.
pub(crate) struct TestResidue {
    /// `None` for a suite whose scopes are minted where it cannot name them —
    /// a warm pool's capacity, or a run under its own realization namespace.
    /// Only the process key applies to those.
    scope: Option<String>,
    backend: Backend,
}

impl TestResidue {
    /// Arm this *after* the suite's daemon readiness check, so a machine
    /// without the daemon stays silent.
    pub(crate) fn arm(scope: impl Into<String>, backend: Backend) -> Self {
        let armed = Self {
            scope: Some(scope.into()),
            backend,
        };
        armed.sweep_scope();
        armed
    }

    /// Claim everything this one-test process causes, for a suite that cannot
    /// name its own scopes. There is nothing to sweep on the way in: a scope
    /// this suite cannot name is also one it cannot collide with.
    #[allow(dead_code)]
    pub(crate) fn for_this_process(backend: Backend) -> Self {
        Self {
            scope: None,
            backend,
        }
    }

    /// Remove the object this scope names, through the one removal that is
    /// addressable by scope alone. Absence is success in both backends, so a
    /// clean scope is silent.
    fn sweep_scope(&self) {
        let Some(scope) = self.scope.clone() else {
            return;
        };
        let backend = self.backend;
        swept_off_thread(move || {
            use awaken_sandbox_container::ContainerRuntime as _;
            let Some(runtime) = sweep_runtime(&scope) else {
                return;
            };
            runtime.block_on(async {
                let outcome = match backend {
                    #[cfg(feature = "docker")]
                    Backend::Docker => {
                        match awaken_sandbox_container::docker::DockerRuntime::connect_local(
                            RESIDUE_AGENT_PORT,
                        ) {
                            Ok(docker) => docker.remove_unreceipted_scope(&scope).await,
                            Err(error) => {
                                eprintln!("residue sweep for `{scope}` has no client: {error}");
                                return;
                            }
                        }
                    }
                    #[cfg(not(feature = "docker"))]
                    Backend::Docker => {
                        unreachable!("Docker residue requires the docker feature")
                    }
                    #[cfg(feature = "podman")]
                    Backend::Podman => {
                        awaken_sandbox_container::podman::PodmanRuntime::new(RESIDUE_AGENT_PORT)
                            .remove_unreceipted_scope(&scope)
                            .await
                    }
                    #[cfg(not(feature = "podman"))]
                    Backend::Podman => {
                        unreachable!("Podman residue requires the podman feature")
                    }
                };
                if let Err(error) = outcome {
                    eprintln!("residue sweep for `{scope}` failed: {error}");
                }
            });
        });
    }

    /// Remove every managed container this process caused, whatever scope it
    /// was created at.
    ///
    /// Scoped to this process on purpose: a container some other process owns
    /// may be a live Session, and no test may remove one of those. Under
    /// nextest each test is its own process, so this is exactly the set one
    /// test made.
    fn sweep_this_process(&self) {
        let cli = self.backend.cli();
        let mine = format!("{}-", std::process::id());
        let Ok(listed) = Command::new(cli)
            .args([
                "ps",
                "-a",
                "--no-trunc",
                "--filter",
                &format!("label={MANAGED_LABEL}"),
                "--format",
                OWNER_LISTING,
            ])
            .output()
        else {
            return;
        };
        for line in String::from_utf8_lossy(&listed.stdout).lines() {
            let Some((id, owner)) = line.trim().split_once(' ') else {
                continue;
            };
            if owner.starts_with(&mine) {
                let _ = Command::new(cli).args(["rm", "-f", id]).output();
            }
        }
    }
}

impl Drop for TestResidue {
    fn drop(&mut self) {
        if self.scope.is_some() {
            self.sweep_scope();
        } else {
            self.sweep_this_process();
        }
    }
}

/// A container this test made the daemon create.
///
/// The factory hands this back rather than the bare id so a suite cannot own
/// a host object without also owning its removal: the id is reachable only
/// through the value that cleans it up.
// Suites that create through a provider rather than the runtime port do not
// reach these, so they are unused in those binaries.
#[allow(dead_code)]
pub(crate) struct OwnedContainer {
    id: String,
    _residue: TestResidue,
}

#[allow(dead_code)]
impl OwnedContainer {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

/// Create one container at `scope` and keep it owned.
#[allow(dead_code)]
pub(crate) async fn create_owned<R: awaken_sandbox_container::ContainerRuntime>(
    runtime: &R,
    scope: &str,
    plan: &awaken_sandbox_container::ContainerPlan,
    backend: Backend,
) -> OwnedContainer {
    let residue = TestResidue::arm(scope, backend);
    let id = runtime
        .create(scope, plan)
        .await
        .expect("create the test's container");
    OwnedContainer {
        id,
        _residue: residue,
    }
}

/// Run one blocking removal on a thread of its own.
///
/// `Drop` cannot await, and it may well be running inside the test's own
/// runtime as that runtime unwinds, so neither the caller's executor nor a
/// client built on it can be reused here. A dedicated thread with a dedicated
/// runtime and a freshly built client owes nothing to the state being torn
/// down. Joining keeps the removal ordered before the test binary exits.
fn swept_off_thread(body: impl FnOnce() + Send + 'static) {
    if std::thread::spawn(body).join().is_err() {
        eprintln!("residue sweep thread panicked");
    }
}

/// The published-agent port plays no part in a container's physical name, so
/// any value addresses the same object the suite created.
const RESIDUE_AGENT_PORT: u16 = 8080;

fn sweep_runtime(scope: &str) -> Option<tokio::runtime::Runtime> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            eprintln!("residue sweep for `{scope}` has no runtime: {error}");
            None
        }
    }
}
