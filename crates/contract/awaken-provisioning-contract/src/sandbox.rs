//! The provisioning ports: [`SandboxProvider`] realizes a [`SandboxSpec`] into a
//! live [`Sandbox`]; [`Sandbox`] launches processes and moves files. Concrete
//! backends (lexical / bubblewrap / container) implement these in their own
//! crates and are selected by [`SandboxCapabilities`].


mod foundation;
pub use foundation::*;
mod runtime;
pub use runtime::*;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::spec::{Command, SandboxSpec};
use crate::vocab::{Artifact, MountAccess, MountRequirement, Realization, RealizedMount};

mod control_incarnation;
pub use control_incarnation::{KubernetesPodUid, SandboxControlIncarnation};
mod repository_publication;
pub use repository_publication::{
    RepositoryPublicationError, RepositoryPublicationExpectation, RepositoryPublicationReceipt,
    RepositoryPublicationRejection,
};

/// A serializable, **durable** reference to a realized sandbox. Persist it the
/// moment a sandbox is created; a live `Box<dyn Sandbox>` cannot survive a host
/// restart, but the handle can be stored and later passed to
/// [`SandboxProvider::adopt`] to reconnect to a still-running remote sandbox
/// (k8s pod / container on another host). For a local sandbox it is just the
/// directory id. The closed, versioned payload enum makes every durable locator
/// explicit and rejects unknown or cross-provider shapes during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxHandle {
    pub sandbox_id: String,
    /// Exact restore effect which owns this physical binding.
    ///
    /// This is provider evidence carried by the canonical durable locator, not
    /// a second lifecycle state. Legacy/create/adopt handles omit it. A
    /// checkpoint-capable provider attaches it while acquiring the one exact
    /// target. The field proves physical identity only; the checkpoint
    /// decorator returns [`SandboxRestoreResult`] after it has materialized and
    /// verified its own bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restoration: Option<SandboxRestorationEvidence>,
    payload: SandboxHandlePayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "schema", rename_all = "snake_case", deny_unknown_fields)]
enum SandboxHandlePayload {
    Unmanaged { provider_kind: String },
    LocalV1(LocalSandboxHandleV1),
    BubblewrapV1(NamespaceSandboxHandleV1),
    SeatbeltV1(NamespaceSandboxHandleV1),
    ContainerV1(ContainerSandboxHandleV1),
}

mod restore_contract;
mod restore_wire;
pub use restore_contract::{
    SandboxRestoreRequest, SandboxRestoreResult, SandboxRestoreTarget,
    SandboxRestoreTargetDisposition, checkpoint_exclusions_fingerprint,
    sandbox_spec_security_fingerprint, validate_checkpoint_exclusions_for_spec,
};
pub use restore_wire::{HostBindRestorationHandle, SandboxRestorationEvidence};

#[cfg(test)]
mod restore_wire_tests;

#[cfg(test)]
mod tests {
    //! A trivial fake exercises the full lifecycle — create → persist handle →
    //! (simulated host restart) adopt → spawn → poll → renew_lease → dispose —
    //! which also proves the ports stay object-safe (`Box<dyn …>`).

    use super::*;
    use crate::spec::{Command, SandboxSpec};
    use crate::vocab::NetworkPolicy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FakeProcess {
        id: String,
    }

    #[async_trait]
    impl ProcessHandle for FakeProcess {
        fn id(&self) -> &str {
            &self.id
        }
        async fn wait(&self) -> Result<ExitStatus, SandboxError> {
            Ok(ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }
        async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
            Ok(Some(ExitStatus {
                code: Some(0),
                signaled: false,
            }))
        }
        async fn signal(&self, _signal: Signal) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeSandbox {
        id: String,
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: "proc-1".into(),
            }))
        }
        async fn attach(&self, _req: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::new("fake has no mounts"))
        }
        async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
            Ok(Vec::new())
        }
        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }
        fn realized(&self) -> &[RealizedMount] {
            &[]
        }
        async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: process_id.into(),
            }))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            self.renews.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn dispose(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeProvider {
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl SandboxProvider for FakeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                isolation: IsolationClass::Workdir,
                tool_transparent: false,
                path_fidelity: false,
                enforced_readonly: false,
                network_isolation: false,
                enforced_network_allowlist: false,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
                package_provisioning: false,
                control_services: Default::default(),
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: spec.scope.clone(),
                renews: self.renews.clone(),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                renews: self.renews.clone(),
            }))
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "thread-1".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: crate::FilesystemContinuity::Retained,
            lease_ttl_secs: Some(60),
            control_services: Default::default(),
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    fn caps(isolation: IsolationClass, network_isolation: bool) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation,
            enforced_network_allowlist: network_isolation,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: Default::default(),
        }
    }

    #[test]
    fn sandbox_requirement_derivation_and_admission_decision_table() {
        // Cause/effect graph:
        // C1=opaque child; C2=requested isolation; C3=read-only mount;
        // C4=restricted/allowlisted network; C5=limits; C6=custom rootfs;
        // C7=packages; C8=Sandbox control-service set. Effects: E1=minimum monotonic requirement vector;
        // E2=one capability predicate accepts every axis; E3=missing any required
        // axis rejects. Constraints: opaque raises isolation to Namespace and
        // requires transparent paths; Allowlist implies network isolation.
        //
        // Decision table:
        // R1 !C1&&!C2..C8 -> Workdir requirement, basic provider accepts.
        // R2 C1 -> Namespace+transparent+path-fidelity.
        // R3 C2..C8 -> every declared enforcement bit/set is required.
        // R4 R3 and one missing capability -> reject; full vector -> accept.
        let bare = spec();
        let r1 = SandboxRequirements::from_spec(&bare, false);
        assert_eq!(r1, SandboxRequirements::default(), "R1");

        let r2 = SandboxRequirements::from_spec(&bare, true);
        assert_eq!(r2.isolation, IsolationClass::Namespace, "R2 isolation");
        assert!(r2.tool_transparent && r2.path_fidelity, "R2 paths");

        let mut demanding = bare;
        demanding.isolation = IsolationClass::Container;
        demanding.mounts.push(crate::vocab::MountRequirement {
            mount_id: "input".into(),
            source: crate::vocab::MountSource::File {
                file_id: "file".into(),
                content_hash: None,
            },
            mount_path: "/workspace/input".into(),
            access: crate::vocab::MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::Session,
            required: true,
        });
        demanding.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.example.test".into()],
        };
        demanding.limits.memory_bytes = Some(64 * 1024 * 1024);
        demanding
            .packages
            .managers
            .insert("npm".into(), vec!["tsx@4".into()]);
        demanding.environment = Some(crate::EnvironmentKind::Image {
            reference: "image@sha256:1".into(),
        });
        demanding
            .control_services
            .insert(awaken_sandbox_control::SandboxControlServiceKind::RepositoryGitCredential);
        let r3 = SandboxRequirements::from_spec(&demanding, true);
        assert_eq!(r3.isolation, IsolationClass::Container, "R3 isolation");
        assert!(
            r3.tool_transparent
                && r3.path_fidelity
                && r3.enforced_readonly
                && r3.network_isolation
                && r3.enforced_network_allowlist
                && r3.resource_limits
                && r3.custom_rootfs
                && r3.package_provisioning
                && r3.control_services == demanding.control_services,
            "R3 vector: {r3:?}"
        );

        let mut full = caps(IsolationClass::Container, true);
        full.custom_rootfs = true;
        full.package_provisioning = true;
        full.control_services = demanding.control_services.clone();
        assert!(full.satisfies_requirements(&r3), "R4 full");
        for missing in [
            "tool_transparent",
            "path_fidelity",
            "enforced_readonly",
            "network_isolation",
            "enforced_network_allowlist",
            "resource_limits",
            "custom_rootfs",
            "package_provisioning",
            "control_services",
        ] {
            let mut weak = full.clone();
            match missing {
                "tool_transparent" => weak.tool_transparent = false,
                "path_fidelity" => weak.path_fidelity = false,
                "enforced_readonly" => weak.enforced_readonly = false,
                "network_isolation" => weak.network_isolation = false,
                "enforced_network_allowlist" => weak.enforced_network_allowlist = false,
                "resource_limits" => weak.resource_limits = false,
                "custom_rootfs" => weak.custom_rootfs = false,
                "package_provisioning" => weak.package_provisioning = false,
                "control_services" => weak.control_services.clear(),
                _ => unreachable!(),
            }
            assert!(!weak.satisfies_requirements(&r3), "R4 missing {missing}");
        }
    }

    #[test]
    fn satisfies_requires_meeting_or_exceeding_isolation() {
        let mut s = spec();
        s.isolation = IsolationClass::Namespace;
        // exact and stronger classes satisfy
        assert!(caps(IsolationClass::Namespace, false).satisfies(&s));
        assert!(caps(IsolationClass::Container, false).satisfies(&s));
        // weaker fails closed
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[test]
    fn satisfies_requires_network_isolation_for_restricted_egress() {
        let mut s = spec();
        s.network = NetworkPolicy::None;
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
        assert!(caps(IsolationClass::Workdir, true).satisfies(&s));
        // unrestricted egress needs no network isolation
        s.network = NetworkPolicy::Unrestricted;
        assert!(caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[test]
    fn satisfies_requires_resource_limit_enforcement_when_limits_are_set() {
        let mut s = spec();
        s.limits.memory_bytes = Some(256 * 1024 * 1024);
        // A tier that can't cgroup fails closed; one that can passes.
        let mut weak = caps(IsolationClass::Workdir, false);
        weak.resource_limits = false;
        assert!(!weak.satisfies(&s));
        let mut strong = caps(IsolationClass::Workdir, false);
        strong.resource_limits = true;
        assert!(strong.satisfies(&s));
        // Unset limits don't require enforcement.
        s.limits = Default::default();
        assert!(weak.satisfies(&s));
    }

    #[test]
    fn satisfies_requires_network_isolation_for_an_allowlist_too() {
        // An Allowlist is restricted (rank 1), so it needs network isolation just like
        // `None` — the middle egress class the other satisfies tests skipped.
        let mut s = spec();
        s.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
        assert!(caps(IsolationClass::Workdir, true).satisfies(&s));
    }

    #[test]
    fn satisfies_rejects_isolation_without_enforced_allowlist() {
        let mut s = spec();
        s.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        let mut isolated = caps(IsolationClass::Container, true);
        isolated.enforced_network_allowlist = false;
        assert!(!isolated.satisfies(&s));
        isolated.enforced_network_allowlist = true;
        assert!(isolated.satisfies(&s));
    }

    /// A provider whose readiness probe can be toggled, to exercise `select_provider`.
    struct ProbeProvider {
        caps: SandboxCapabilities,
        ready: bool,
    }
    #[async_trait]
    impl SandboxProvider for ProbeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            self.caps.clone()
        }
        async fn probe_ready(&self) -> Result<(), SandboxError> {
            if self.ready {
                Ok(())
            } else {
                Err(SandboxError::new("backend not ready"))
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: spec.scope.clone(),
                renews: Arc::new(AtomicU32::new(0)),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                renews: Arc::new(AtomicU32::new(0)),
            }))
        }
    }

    #[tokio::test]
    async fn select_provider_picks_the_first_capable_and_ready_backend() {
        let mut s = spec();
        s.isolation = IsolationClass::Namespace;
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            // capable but not ready → skipped
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: false,
            }),
            // capable AND ready → chosen
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let chosen = select_provider(&candidates, &s).await.unwrap();
        assert_eq!(chosen.capabilities().isolation, IsolationClass::Namespace);
    }

    #[tokio::test]
    async fn select_provider_fails_closed_rather_than_downgrading() {
        let mut s = spec();
        s.isolation = IsolationClass::Container;
        // Only an under-isolating and a not-ready backend are offered.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: false,
            }),
        ];
        let result = select_provider(&candidates, &s).await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn select_provider_with_no_candidates_fails_closed() {
        // Boundary: an empty candidate list never downgrades to an unisolated run.
        let candidates: Vec<Box<dyn SandboxProvider>> = Vec::new();
        let result = select_provider(&candidates, &spec()).await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    // --- IsolationPolicy (ADR-0056 §5): the floor is a policy input. Decision table
    // over (floor met? × on_unmet × candidates), preserving never-downgrade for
    // FailClosed and making DegradeWithConsent a recorded, non-silent placement.

    fn policy(
        require: IsolationClass,
        prefer: IsolationClass,
        on_unmet: OnUnmet,
    ) -> IsolationPolicy {
        IsolationPolicy {
            require,
            prefer,
            on_unmet,
        }
    }

    #[tokio::test]
    async fn policy_places_at_the_floor_and_prefers_the_preferred_tier() {
        // require=Namespace floor is met by both Namespace and Container; prefer=Namespace
        // picks the exact preferred tier (not the strongest), and it is not degraded.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let sel = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Namespace,
                IsolationClass::Namespace,
                OnUnmet::FailClosed,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            sel.provider.capabilities().isolation,
            IsolationClass::Namespace
        );
        assert_eq!(
            sel.degraded_to, None,
            "a floor-meeting placement is not a degrade"
        );
    }

    #[tokio::test]
    async fn policy_fail_closed_refuses_when_the_floor_is_unmet() {
        // require=Container, only Namespace available, FailClosed → never downgrade.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: caps(IsolationClass::Namespace, true),
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::FailClosed,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn policy_degrade_with_consent_places_below_the_floor_and_records_it() {
        // require=Container, only Namespace + Workdir available, DegradeWithConsent →
        // the STRONGEST below-floor tier (Namespace) is placed, and degraded_to reports
        // it so the caller emits the audit/metric/marker.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Workdir, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let sel = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            sel.provider.capabilities().isolation,
            IsolationClass::Namespace
        );
        assert_eq!(
            sel.degraded_to,
            Some(IsolationClass::Namespace),
            "a consented degrade is recorded, never silent"
        );
    }

    #[tokio::test]
    async fn policy_degrade_with_consent_still_fails_closed_with_no_backend_at_all() {
        // Even DegradeWithConsent cannot place a run with zero ready backends.
        let candidates: Vec<Box<dyn SandboxProvider>> = Vec::new();
        let result = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn policy_excludes_a_backend_that_cannot_enforce_the_specs_limits() {
        // A spec asking for cgroup limits must not be placed on a tier that cannot
        // enforce them — even under a strong isolation class, `non_isolation_ok` bars it.
        let mut s = spec();
        s.limits.memory_bytes = Some(512 * 1024 * 1024);
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: SandboxCapabilities {
                resource_limits: false, // strong isolation but cannot enforce limits
                ..caps(IsolationClass::Container, true)
            },
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &s,
            &policy(
                IsolationClass::Workdir,
                IsolationClass::Workdir,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await;
        assert!(
            matches!(result, Err(SelectionError::NoCapableBackend)),
            "a limits-incapable backend is excluded even from the degrade set"
        );
    }

    #[tokio::test]
    async fn policy_excludes_a_backend_that_cannot_meet_the_specs_network() {
        // A restricted-egress spec needs network isolation; a non-isolating backend is
        // excluded from the floor set even if its isolation class qualifies.
        let mut s = spec();
        s.network = NetworkPolicy::None;
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: caps(IsolationClass::Container, false), // no network isolation
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &s,
            &policy(
                IsolationClass::Workdir,
                IsolationClass::Workdir,
                OnUnmet::FailClosed,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[test]
    fn every_sandbox_status_variant_round_trips_on_the_wire() {
        // Status crosses the reconnect boundary (queried idempotently after a takeover),
        // so its wire tags are load-bearing. `Provisioning`/`Terminated` were never
        // exercised (fakes always return `Ready`).
        for (s, tag) in [
            (SandboxStatus::Provisioning, "provisioning"),
            (SandboxStatus::Ready, "ready"),
            (SandboxStatus::Terminated, "terminated"),
        ] {
            let wire = serde_json::to_string(&s).unwrap();
            assert_eq!(wire, format!("\"{tag}\""));
            assert_eq!(serde_json::from_str::<SandboxStatus>(&wire).unwrap(), s);
        }
    }

    /// Cause-effect graph for mediated secret custody:
    ///
    /// C1 provider substitutes the secret at egress
    /// C2 provider enforces a no-bypass target allowlist
    /// E1 real material may remain outside the workload iff C1 AND C2.
    ///
    /// | Rule | C1 substitution | C2 no-bypass | E1 custody evidence |
    /// |---|---|---|---|
    /// | E1 | F | F | F |
    /// | E2 | T | F | F |
    /// | E3 | F | T | F |
    /// | E4 | T | T | T |
    #[test]
    fn secret_egress_custody_requires_substitution_and_no_bypass() {
        for (substitution, no_bypass, expected) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let mut capabilities = caps(IsolationClass::Container, true);
            capabilities.secret_egress_substitution = substitution;
            capabilities.enforced_network_allowlist = no_bypass;
            assert_eq!(
                capabilities.supports_secret_egress_without_bypass(),
                expected,
                "substitution={substitution}, no_bypass={no_bypass}"
            );
        }
    }

    #[test]
    fn a_signal_killed_exit_status_round_trips() {
        // A process reaped by a signal has no exit code and `signaled = true` — the
        // shape every fake elided by always returning `code: Some(0)`.
        let killed = ExitStatus {
            code: None,
            signaled: true,
        };
        let wire = serde_json::to_string(&killed).unwrap();
        assert_eq!(serde_json::from_str::<ExitStatus>(&wire).unwrap(), killed);
        assert!(killed.code.is_none() && killed.signaled);
    }

    #[test]
    fn every_signal_variant_round_trips_on_the_wire() {
        // Only `Term` was ever delivered in a test; pin all three wire tags.
        for (sig, tag) in [
            (Signal::Term, "term"),
            (Signal::Kill, "kill"),
            (Signal::Int, "int"),
        ] {
            let wire = serde_json::to_string(&sig).unwrap();
            assert_eq!(wire, format!("\"{tag}\""));
            assert_eq!(serde_json::from_str::<Signal>(&wire).unwrap(), sig);
        }
    }

    #[tokio::test]
    async fn default_probe_ready_is_ok() {
        let provider = FakeProvider {
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert!(provider.probe_ready().await.is_ok());
    }

    #[tokio::test]
    async fn create_persist_adopt_poll_lease_lifecycle() {
        let renews = Arc::new(AtomicU32::new(0));
        let provider: Box<dyn SandboxProvider> = Box::new(FakeProvider {
            renews: renews.clone(),
        });

        // Create, then persist the durable handle (as a host would to its store).
        let sandbox = provider.create(&spec()).await.unwrap();
        let handle = sandbox.handle();
        let wire = serde_json::to_string(&handle).unwrap(); // handle is serializable
        drop(sandbox); // simulate the owning host process going away

        // Recovery: reconnect from the persisted handle alone.
        let recovered: SandboxHandle = serde_json::from_str(&wire).unwrap();
        let sandbox = provider.adopt(&recovered).await.unwrap();
        assert_eq!(sandbox.id(), "thread-1");
        assert!(matches!(
            sandbox.status().await.unwrap(),
            SandboxStatus::Ready
        ));

        // Launch an opaque process (e.g. Claude Code), then resolve its outcome
        // idempotently via poll — the reconnect-safe path.
        let proc = sandbox
            .spawn(Command::new(["claude", "--acp"]))
            .await
            .unwrap();
        let reattached = sandbox.process(proc.id()).await.unwrap();
        assert!(matches!(
            reattached.poll().await.unwrap(),
            Some(ExitStatus { code: Some(0), .. })
        ));

        sandbox.renew_lease().await.unwrap();
        assert_eq!(renews.load(Ordering::SeqCst), 1);
        sandbox.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn fixtures_conform_to_the_ports() {
        // The selection/lifecycle tests above don't drive every port method; assert
        // the fakes are well-formed contract impls (a valid `Sandbox`/`ProcessHandle`/
        // `SandboxProvider`) so the parts they *do* rely on rest on a sound fixture.
        let sb = FakeSandbox {
            id: "s".into(),
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert_eq!(sb.id(), "s");
        assert!(sb.attach(a_mount()).await.is_err());
        assert!(sb.artifacts().await.unwrap().is_empty());
        assert!(sb.read_artifact("x").await.unwrap().is_empty());
        assert!(sb.realized().is_empty());

        let proc = FakeProcess { id: "p".into() };
        assert_eq!(proc.id(), "p");
        assert_eq!(proc.wait().await.unwrap().code, Some(0));
        proc.signal(Signal::Term).await.unwrap();

        // The Workdir fake provider (used by `default_probe_ready_is_ok`) and the
        // toggleable ProbeProvider both realize the same ports.
        let fp = FakeProvider {
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert_eq!(fp.capabilities().isolation, IsolationClass::Workdir);

        let pp = ProbeProvider {
            caps: caps(IsolationClass::Container, true),
            ready: true,
        };
        assert!(pp.create(&spec()).await.is_ok());
        assert_eq!(
            pp.adopt(&SandboxHandle::new("k", "id")).await.unwrap().id(),
            "id"
        );
    }

    fn a_mount() -> MountRequirement {
        MountRequirement {
            mount_id: "m".into(),
            source: crate::vocab::MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            mount_path: "/workspace/x".into(),
            access: crate::vocab::MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: false,
        }
    }
}
