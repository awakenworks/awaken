//! Residue containment for the live-daemon suites.
//!
//! A live-daemon test owns physical objects the daemon keeps after the test
//! process is gone, and a Sandbox's physical name is a pure function of its
//! scope. So a test that dies before its own teardown does not merely leak: the
//! *next* run of that same test meets its own leftover, and creation fails
//! closed against an existing realization this attempt holds no fence for.
//! That is how one rare flake became a permanent failure that only a manual
//! `docker rm -f` could clear.
//!
//! The guard closes both sides of that loop with the one removal that is
//! addressable by scope alone. Arming it sweeps before the test creates
//! anything, so leftovers from any earlier run are already gone; dropping it
//! sweeps again, and Drop runs while a panic unwinds — precisely the path that
//! used to leak.

/// Removes one scope's physical object when armed and again when dropped.
pub(crate) struct ScopeResidue {
    scope: String,
    sweep: fn(&str),
}

impl ScopeResidue {
    /// Sweep `scope` now and again on drop. Arm this *after* the suite's daemon
    /// readiness check so a machine without the daemon stays silent.
    pub(crate) fn arm(scope: impl Into<String>, sweep: fn(&str)) -> Self {
        let scope = scope.into();
        sweep(&scope);
        Self { scope, sweep }
    }
}

impl Drop for ScopeResidue {
    fn drop(&mut self) {
        (self.sweep)(&self.scope);
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
#[cfg(any(feature = "docker", feature = "podman"))]
const RESIDUE_AGENT_PORT: u16 = 8080;

/// Remove the Docker container this scope names, if the daemon still holds one.
// A suite arms one backend, so the other's sweep is unreferenced in its binary.
#[allow(dead_code)]
#[cfg(feature = "docker")]
pub(crate) fn docker(scope: &str) {
    let scope = scope.to_string();
    swept_off_thread(move || {
        use awaken_sandbox_container::ContainerRuntime as _;
        let Some(runtime) = sweep_runtime(&scope) else {
            return;
        };
        runtime.block_on(async {
            match awaken_sandbox_container::docker::DockerRuntime::connect_local(RESIDUE_AGENT_PORT)
            {
                Ok(docker) => report(&scope, docker.remove_unreceipted_scope(&scope).await),
                Err(error) => {
                    eprintln!("residue sweep for `{scope}` has no Docker client: {error}");
                }
            }
        });
    });
}

/// Remove the Podman container this scope names, if one still exists.
#[allow(dead_code)]
#[cfg(feature = "podman")]
pub(crate) fn podman(scope: &str) {
    let scope = scope.to_string();
    swept_off_thread(move || {
        use awaken_sandbox_container::ContainerRuntime as _;
        let Some(runtime) = sweep_runtime(&scope) else {
            return;
        };
        let podman = awaken_sandbox_container::podman::PodmanRuntime::new(RESIDUE_AGENT_PORT);
        runtime.block_on(async { report(&scope, podman.remove_unreceipted_scope(&scope).await) });
    });
}

#[cfg(any(feature = "docker", feature = "podman"))]
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

/// A scope the daemon never held is already swept, so the backends report
/// absence as success and only a real removal failure is worth printing.
#[cfg(any(feature = "docker", feature = "podman"))]
fn report(scope: &str, outcome: Result<(), awaken_sandbox_container::RuntimeError>) {
    if let Err(error) = outcome {
        eprintln!("residue sweep for `{scope}` failed: {error}");
    }
}
