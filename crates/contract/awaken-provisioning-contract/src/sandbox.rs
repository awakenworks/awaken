//! The provisioning ports: [`SandboxProvider`] realizes a [`crate::SandboxSpec`] into a
//! live [`Sandbox`]; [`Sandbox`] launches processes and moves files. Concrete
//! backends (lexical / bubblewrap / container) implement these in their own
//! crates and are selected by [`SandboxCapabilities`].

mod control_incarnation;
mod foundation;
mod repository_publication;
mod restore_contract;
mod restore_wire;
mod runtime;

pub use control_incarnation::{KubernetesPodUid, SandboxControlIncarnation};
pub use foundation::*;
pub use repository_publication::{
    RepositoryPublicationError, RepositoryPublicationExpectation, RepositoryPublicationReceipt,
    RepositoryPublicationRejection,
};
pub use restore_contract::{
    SandboxRestoreRequest, SandboxRestoreResult, SandboxRestoreTarget,
    SandboxRestoreTargetDisposition, checkpoint_exclusions_fingerprint,
    sandbox_spec_security_fingerprint, validate_checkpoint_exclusions_for_spec,
};
pub use restore_wire::{HostBindRestorationHandle, SandboxRestorationEvidence};
pub use runtime::*;

#[cfg(test)]
mod restore_wire_tests;

#[cfg(test)]
use crate::vocab::{Artifact, MountAccess, MountRequirement, RealizedMount};
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
mod tests {
    //! A trivial fake exercises the full lifecycle — create → persist handle →
    //! (simulated host restart) adopt → spawn → poll → renew_lease → dispose —
    //! which also proves the ports stay object-safe (`Box<dyn …>`).

    use super::*;
    use crate::SandboxRealizationFingerprint;
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

    #[test]
    fn repository_mount_path_is_exact_or_rejected_without_relocation() {
        // Repository-path cause/effect decision table:
        // C1 canonical `/workspace/<child>` -> E1 accept the exact bytes;
        // C2 another absolute root or a relative path -> E2 reject, never remap;
        // C3 alias/traversal/empty/control syntax -> E3 reject before credentials/Git;
        // C4 a runtime-owned root or descendant -> E4 reject before a Runtime
        // projection can clear/overwrite the Repository; C5 an ordinary hidden
        // repository such as `.github` -> E5 accept (no blanket dot-path ban).
        // Constraint: validation is pure and may not mutate durable mount_path.
        let plan = RepositoryRealizationPlan {
            repository_id: "repository-a".into(),
            mount_path: "/workspace/repository-a".into(),
            source_remote_url: "https://example.invalid/repository-a.git".into(),
            transport_url: "https://gateway.invalid/git/repository-a".into(),
            initial_branch: None,
            initial_commit: None,
            access: MountAccess::ReadWrite,
        };
        plan.validate_mount_path().expect("C1/E1");
        assert_eq!(plan.mount_path, "/workspace/repository-a", "C1 exact");
        let hidden_repository = RepositoryRealizationPlan {
            mount_path: "/workspace/.github".into(),
            ..plan.clone()
        };
        hidden_repository.validate_mount_path().expect("C5/E5");

        for mount_path in [
            "/repo",
            "repo",
            "/workspace",
            "/workspace/",
            "/workspace//repo",
            "/workspace/./repo",
            "/workspace/repo/../escape",
            "/workspace/repo\\child",
            "/workspace/repo\nchild",
            "/workspace/repo\rchild",
            "/workspace/repo\tchild",
            "/workspace/repo\u{1b}child",
            "/workspace/.mnt",
            "/workspace/.mnt/repository",
            "/workspace/.skills/repository",
            "/workspace/.acp-config/repository",
            "/workspace/.config/repository",
            "/workspace/.cache/repository",
            "/workspace/.codex/repository",
            "/workspace/.awaken/repository",
        ] {
            let invalid = RepositoryRealizationPlan {
                mount_path: mount_path.into(),
                ..plan.clone()
            };
            assert!(
                invalid.validate_mount_path().is_err(),
                "C2-C4/E2-E4 accepted {mount_path:?}"
            );
            assert_eq!(invalid.mount_path, mount_path, "no normalization/remap");
        }
    }

    #[test]
    fn resource_input_default_mounts_cover_the_closed_input_kind_set() {
        // Default-mount cause/effect decision table:
        // | Rule | typed input kind | Effect |
        // | D1 | MemoryStore | `/mnt/memory` |
        // | D2 | File | `/mnt/files/data` |
        // | D3 | Repository | `WorkspaceLayout::child("repo")` |
        // Constraint: the returned value object is the sole default authority;
        // consumers may project it but may not restate one of these paths.
        let defaults = resource_input_default_mounts();
        let cases = [
            (
                "D1",
                awaken_resource_contract::InputResourceId::MemoryStore(
                    awaken_resource_contract::MemoryStoreId::from("memory"),
                ),
                "/mnt/memory".to_string(),
            ),
            (
                "D2",
                awaken_resource_contract::InputResourceId::File(
                    awaken_resource_contract::FileId::from("file"),
                ),
                "/mnt/files/data".to_string(),
            ),
            (
                "D3",
                awaken_resource_contract::InputResourceId::Repository(
                    awaken_resource_contract::RepositoryId::from("repository"),
                ),
                WorkspaceLayout::child("repo"),
            ),
        ];
        for (rule, target, expected) in cases {
            assert_eq!(defaults.mount_path(&target), expected, "{rule}");
        }
        assert_eq!(defaults.repository, WorkspaceLayout::child("repo"), "D3");
    }

    #[test]
    fn repository_mount_trees_never_overlap_another_resource() {
        // Tree-ownership cause/effect rules: C1 disjoint Repository/File trees
        // -> E1 accept; C2 Repository-Repository ancestor relation -> E2 reject;
        // C3 Repository-other ancestor relation in either direction -> E3 reject.
        // Constraint: segment boundaries matter (`repo` and `repository` are
        // disjoint) and the preflight performs no realization effect.
        validate_repository_mount_paths(
            &["/workspace/repo", "/workspace/repository"],
            &["/workspace/input.txt"],
        )
        .expect("C1/E1");
        assert!(
            validate_repository_mount_paths(&["/workspace/repo", "/workspace/repo/nested"], &[],)
                .is_err(),
            "C2/E2"
        );
        for other in ["/workspace/repo/file", "/workspace"] {
            assert!(
                validate_repository_mount_paths(&["/workspace/repo"], &[other]).is_err(),
                "C3/E3: {other}"
            );
        }
    }

    #[test]
    fn workspace_layout_projects_output_paths_without_a_second_root_literal() {
        // Layout cause/effect table: C1=the exact outputs root; C2=a canonical
        // descendant; C3=a prefix lookalike. E1=empty relative path;
        // E2=the exact descendant suffix; E3=no projection. R1 C1->E1;
        // R2 C2->E2; R3 C3->E3. Consumers must not restate the root.
        assert_eq!(
            WorkspaceLayout::outputs_relative(WorkspaceLayout::OUTPUTS_ROOT),
            Some(""),
            "R1/E1"
        );
        assert_eq!(
            WorkspaceLayout::outputs_relative("/mnt/session/outputs/nested/report.txt"),
            Some("nested/report.txt"),
            "R2/E2"
        );
        assert_eq!(
            WorkspaceLayout::outputs_relative("/mnt/session/outputs-other/report.txt"),
            None,
            "R3/E3"
        );
    }

    #[test]
    fn final_sandbox_layout_rejects_repository_effect_conflicts() {
        // Final-layout cause/effect table:
        // R1 disjoint Repository, mount, outputs and default HOME -> admit;
        // R2 mount/extra-mount is inside Repository -> reject;
        // R3 outputs or an explicit runtime home is inside Repository -> reject;
        // R4 duplicate effective mount owners -> reject even without Repository.
        // R5 aliases, traversal, cross-platform separators, control characters,
        // or relative runtime directories -> reject before overlap comparison.
        // All rules are pure and run before prewarm/provider/Git effects.
        let repository = ["/workspace/repo"];
        let mut layout = spec();
        validate_repository_sandbox_layout(&repository, &layout).expect("R1");

        layout.mounts.push(MountRequirement {
            mount_id: "nested".into(),
            source: crate::vocab::MountSource::InlineBytes {
                contents: Vec::new(),
                content_hash: None,
            },
            mount_path: "/workspace/repo/input".into(),
            access: MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: true,
        });
        assert!(
            validate_repository_sandbox_layout(&repository, &layout).is_err(),
            "R2"
        );
        layout.mounts.clear();

        for (name, path) in [
            ("outputs", "/workspace/repo/outputs"),
            ("HOME", "/workspace/repo/home"),
        ] {
            layout.outputs_path = "/mnt/session/outputs".into();
            layout.env.clear();
            if name == "outputs" {
                layout.outputs_path = path.into();
            } else {
                layout.env.push(crate::vocab::EnvVar {
                    name: name.into(),
                    value: crate::vocab::EnvValue::Inline { value: path.into() },
                    visibility: crate::vocab::EnvVisibility::Process,
                });
            }
            assert!(
                validate_repository_sandbox_layout(&repository, &layout).is_err(),
                "R3 {name}"
            );
        }

        layout.outputs_path = "/mnt/session/outputs".into();
        layout.env.clear();
        let duplicate = MountRequirement {
            mount_id: "one".into(),
            source: crate::vocab::MountSource::InlineBytes {
                contents: Vec::new(),
                content_hash: None,
            },
            mount_path: "/workspace/input".into(),
            access: MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: true,
        };
        layout.mounts = vec![
            duplicate.clone(),
            MountRequirement {
                mount_id: "two".into(),
                ..duplicate
            },
        ];
        assert!(
            validate_repository_sandbox_layout(&[], &layout).is_err(),
            "R4"
        );

        for invalid_mount in [
            "workspace//input",
            "workspace/input/",
            "workspace/./input",
            "workspace/../input",
            "workspace\\input",
            "workspace/input\nchild",
        ] {
            layout.mounts = vec![MountRequirement {
                mount_id: "invalid".into(),
                source: crate::vocab::MountSource::InlineBytes {
                    contents: Vec::new(),
                    content_hash: None,
                },
                mount_path: invalid_mount.into(),
                access: MountAccess::ReadOnly,
                lifetime: crate::vocab::MountLifetime::PerRun,
                required: true,
            }];
            assert!(
                validate_repository_sandbox_layout(&[], &layout).is_err(),
                "R5 mount {invalid_mount:?}"
            );
        }
        layout.mounts.clear();
        for invalid_outputs in [
            "mnt/session/outputs",
            "/mnt/session//outputs",
            "/mnt/session/outputs/",
            "/mnt/session/../outputs",
            "/mnt/session\\outputs",
            "/mnt/session/outputs\nchild",
        ] {
            layout.outputs_path = invalid_outputs.into();
            assert!(
                validate_repository_sandbox_layout(&[], &layout).is_err(),
                "R5 outputs {invalid_outputs:?}"
            );
        }
        layout.outputs_path = "/mnt/session/outputs".into();
        for invalid_directory in ["workspace/home", "/workspace/home/", "/workspace/home\tbad"] {
            layout.env = vec![crate::vocab::EnvVar {
                name: "HOME".into(),
                value: crate::vocab::EnvValue::Inline {
                    value: invalid_directory.into(),
                },
                visibility: crate::vocab::EnvVisibility::Process,
            }];
            assert!(
                validate_repository_sandbox_layout(&[], &layout).is_err(),
                "R5 HOME {invalid_directory:?}"
            );
        }
        layout.env = vec![crate::vocab::EnvVar {
            name: "XDG_CONFIG_HOME".into(),
            value: crate::vocab::EnvValue::Inline {
                value: WorkspaceLayout::ROOT.into(),
            },
            visibility: crate::vocab::EnvVisibility::Process,
        }];
        assert!(
            validate_repository_sandbox_layout(&repository, &layout).is_err(),
            "R3 XDG root owns the Repository tree"
        );
        layout.env = vec![crate::vocab::EnvVar {
            name: "CODEX_HOME".into(),
            value: crate::vocab::EnvValue::Secret {
                reference: "opaque-runtime-directory".into(),
            },
            visibility: crate::vocab::EnvVisibility::Process,
        }];
        assert!(
            validate_repository_sandbox_layout(&repository, &layout).is_err(),
            "R5 opaque runtime directory cannot be compared across providers"
        );
    }

    #[test]
    fn v2_handles_round_trip_complete_owned_path_evidence() {
        // Durable-layout decision table: H1 current Local/Namespace/Container
        // V2 payload => exact owned paths survive wire round-trip; H2 legacy V1
        // => still decodes and exposes no invented evidence. Runtime may adopt
        // H2 only when no Repository requires an overlap proof.
        let local_v1 = LocalSandboxHandleV1 {
            outputs_path: "/outputs".into(),
            base_env: Vec::new(),
            continuation_excluded_paths: Vec::new(),
            deny_tool_egress: false,
        };
        let namespace_v1 = NamespaceSandboxHandleV1 {
            outputs_path: "/outputs".into(),
            base_env: Vec::new(),
            network: crate::NetworkPolicy::None,
            control_services: Default::default(),
        };
        let container_v1 = ContainerSandboxHandleV1 {
            container_id: "container-1".into(),
            outputs_path: "/outputs".into(),
            base_env: Vec::new(),
            live_input_projection: false,
            continuation_excluded_paths: Vec::new(),
            runtime_handle: None,
            sandbox_control_incarnation: None,
            control_services: Default::default(),
        };
        let realization_fingerprint = SandboxRealizationFingerprint::from_spec(&spec());
        let filesystem_fence =
            SandboxEffectFence::new("handle-round-trip", "test-owner", "runtime-1", 1, u64::MAX)
                .unwrap();
        let cases = [
            SandboxHandle::local_v2(
                "local",
                LocalSandboxHandleV2 {
                    previous: local_v1.clone(),
                    realization_fingerprint: realization_fingerprint.clone(),
                    effect_fence: filesystem_fence.clone(),
                    physical_incarnation: "local-incarnation".into(),
                    owned_paths: vec!["/workspace/local-repo".into()],
                },
            ),
            SandboxHandle::namespace_v2(
                NamespaceProviderKind::Bubblewrap,
                "namespace",
                NamespaceSandboxHandleV2 {
                    previous: namespace_v1,
                    realization_fingerprint: realization_fingerprint.clone(),
                    effect_fence: filesystem_fence,
                    physical_incarnation: "namespace-incarnation".into(),
                    owned_paths: vec!["/workspace/namespace-repo".into()],
                },
            ),
            SandboxHandle::container_v2(
                "container",
                ContainerSandboxHandleV2 {
                    previous: container_v1,
                    adoption_fingerprint: realization_fingerprint.clone(),
                    realization_fingerprint,
                    owned_paths: vec!["/workspace/container-repo".into()],
                },
            ),
        ];
        let mut missing_fingerprint = serde_json::to_value(&cases[0]).unwrap();
        missing_fingerprint["payload"]
            .as_object_mut()
            .unwrap()
            .remove("realization_fingerprint");
        assert!(
            serde_json::from_value::<SandboxHandle>(missing_fingerprint).is_err(),
            "H1 current V2 never accepts missing immutable realization evidence"
        );
        let mut missing_adoption = serde_json::to_value(&cases[2]).unwrap();
        missing_adoption["payload"]
            .as_object_mut()
            .unwrap()
            .remove("adoption_fingerprint");
        assert!(
            serde_json::from_value::<SandboxHandle>(missing_adoption).is_err(),
            "H1 current container V2 never accepts missing adoption evidence"
        );
        for handle in cases {
            let expected = handle.owned_paths().expect("H1 V2 evidence").to_vec();
            let decoded: SandboxHandle =
                serde_json::from_str(&serde_json::to_string(&handle).unwrap()).unwrap();
            assert_eq!(decoded.owned_paths(), Some(expected.as_slice()), "H1");
        }

        let legacy = SandboxHandle::local("legacy", local_v1);
        let decoded: SandboxHandle =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert!(decoded.local_payload().is_ok(), "H2 V1 decode");
        assert_eq!(decoded.owned_paths(), None, "H2 no fabricated evidence");
    }

    #[test]
    fn effect_identity_ignores_only_expiry() {
        // Cause/effect table: C1 operation/owner/runtime/epoch are all equal;
        // C2 expiry is equal/renewed; C3 one immutable identity field differs.
        // R1 C1 is the same effect for either C2 value; R2 any C3 difference is
        // another effect. This accessor is the neutral owner used by Host and
        // provider marker validation; it grants no lease-liveness authority.
        let original = SandboxEffectFence::new("effect", "owner", "runtime", 7, 10).unwrap();
        let renewed = SandboxEffectFence::new("effect", "owner", "runtime", 7, 20).unwrap();
        let other = SandboxEffectFence::new("other", "owner", "runtime", 7, 20).unwrap();
        assert!(original.same_effect_identity(&renewed), "R1");
        assert!(!original.same_effect_identity(&other), "R2");
    }

    #[test]
    fn memory_materialization_slice_canonicalization_is_total() {
        // Cause/effect decision table: C1 every item is internally valid;
        // C2 mount/store identities are distinct or duplicated; C3 input order
        // is canonical or reversed. M1 C1+distinct sorts the existing slice by
        // mount then store without changing its length; M2 C1+duplicate
        // rejects; M3 !C1 rejects before durable handle attachment. No rule
        // needs Vec capacity or changes collection membership, so the slice is
        // the sole minimal mutation boundary.
        let evidence = |store: &str, mount: &str| {
            MemoryMaterializationEvidence::new(store, mount, Vec::new()).unwrap()
        };
        let mut reversed = [
            evidence("memory-z", "/workspace/z"),
            evidence("memory-a", "/workspace/a"),
        ];
        MemoryMaterializationEvidence::canonicalize_all(&mut reversed).expect("M1");
        assert_eq!(reversed[0].mount_path, "/workspace/a", "M1 order");
        assert_eq!(reversed.len(), 2, "M1 membership");

        let mut duplicate = [
            evidence("memory", "/workspace/memory"),
            evidence("memory", "/workspace/memory"),
        ];
        assert!(
            MemoryMaterializationEvidence::canonicalize_all(&mut duplicate).is_err(),
            "M2"
        );

        let mut invalid = [evidence("memory", "/workspace/memory")];
        invalid[0].store_id.clear();
        assert!(
            MemoryMaterializationEvidence::canonicalize_all(&mut invalid).is_err(),
            "M3"
        );
    }

    #[test]
    fn resource_reservation_requires_one_v2_substrate_and_monotonic_paths() {
        // Cause/effect decision table: C1 source/target are V2, C2 provider/id/
        // immutable locator+fingerprint+effect fence match, C3 every old owned path remains,
        // C4 original copy-backed Memory heads match exactly. R1 C1+C2+C3+C4
        // admits an exact replay or superset; R2 !C1 rejects legacy
        // V1->V1/V2 evidence minting; R3 !C2 rejects another substrate/spec;
        // R4 !C3 rejects path loss; R5 !C4 rejects a changed terminal CAS base.
        // No rule mutates either durable handle.
        let previous = LocalSandboxHandleV1 {
            outputs_path: "/outputs".into(),
            base_env: Vec::new(),
            continuation_excluded_paths: Vec::new(),
            deny_tool_egress: false,
        };
        let fingerprint = SandboxRealizationFingerprint::from_spec(&spec());
        let fence =
            SandboxEffectFence::new("reservation", "test-owner", "runtime-1", 1, u64::MAX).unwrap();
        let current = SandboxHandle::local_v2(
            "sandbox",
            LocalSandboxHandleV2 {
                previous: previous.clone(),
                realization_fingerprint: fingerprint.clone(),
                effect_fence: fence.clone(),
                physical_incarnation: "incarnation-1".into(),
                owned_paths: vec!["/workspace/a".into()],
            },
        );
        let superset = SandboxHandle::local_v2(
            "sandbox",
            LocalSandboxHandleV2 {
                previous: previous.clone(),
                realization_fingerprint: fingerprint.clone(),
                effect_fence: fence.clone(),
                physical_incarnation: "incarnation-1".into(),
                owned_paths: vec!["/workspace/a".into(), "/workspace/b".into()],
            },
        );
        assert!(current.owned_paths_are_monotonic_to(&current), "R1 replay");
        assert!(
            current.owned_paths_are_monotonic_to(&superset),
            "R1 superset"
        );
        let original_memory = MemoryMaterializationEvidence::new(
            "memory",
            "/workspace/memory",
            vec![MemoryMaterializationHead {
                path: "notes.txt".into(),
                id: "head-a".into(),
                content_sha256: "sha-a".into(),
            }],
        )
        .unwrap();
        let current_with_memory = current
            .clone()
            .with_memory_materializations(vec![original_memory.clone()])
            .unwrap();
        let superset_with_memory = superset
            .clone()
            .with_memory_materializations(vec![original_memory])
            .unwrap();
        assert!(
            current_with_memory.owned_paths_are_monotonic_to(&superset_with_memory),
            "R1 matching Memory authority"
        );
        assert!(
            !current.owned_paths_are_monotonic_to(&superset_with_memory),
            "R5 missing Memory authority"
        );
        let changed_memory = superset
            .clone()
            .with_memory_materializations(vec![
                MemoryMaterializationEvidence::new(
                    "memory",
                    "/workspace/memory",
                    vec![MemoryMaterializationHead {
                        path: "notes.txt".into(),
                        id: "head-b".into(),
                        content_sha256: "sha-b".into(),
                    }],
                )
                .unwrap(),
            ])
            .unwrap();
        assert!(
            !current_with_memory.owned_paths_are_monotonic_to(&changed_memory),
            "R5 changed Memory authority"
        );

        let legacy = SandboxHandle::local("sandbox", previous.clone());
        assert!(!legacy.owned_paths_are_monotonic_to(&legacy), "R2 V1->V1");
        assert!(!legacy.owned_paths_are_monotonic_to(&superset), "R2 V1->V2");

        let mut different_spec = spec();
        different_spec.scope = "different".into();
        let different_spec = SandboxHandle::local_v2(
            "sandbox",
            LocalSandboxHandleV2 {
                previous: previous.clone(),
                realization_fingerprint: SandboxRealizationFingerprint::from_spec(&different_spec),
                effect_fence: fence.clone(),
                physical_incarnation: "incarnation-1".into(),
                owned_paths: vec!["/workspace/a".into(), "/workspace/b".into()],
            },
        );
        assert!(
            !current.owned_paths_are_monotonic_to(&different_spec),
            "R3 spec"
        );
        let different_id = SandboxHandle::local_v2(
            "other",
            LocalSandboxHandleV2 {
                previous,
                realization_fingerprint: fingerprint,
                effect_fence: fence.clone(),
                physical_incarnation: "incarnation-1".into(),
                owned_paths: vec!["/workspace/a".into(), "/workspace/b".into()],
            },
        );
        assert!(
            !current.owned_paths_are_monotonic_to(&different_id),
            "R3 id"
        );

        let different_effect = SandboxHandle::local_v2(
            "sandbox",
            LocalSandboxHandleV2 {
                previous: current.local_payload().unwrap().clone(),
                realization_fingerprint: current.realization_fingerprint().unwrap().clone(),
                effect_fence: SandboxEffectFence::new(
                    "other-reservation",
                    "test-owner",
                    "runtime-1",
                    1,
                    u64::MAX,
                )
                .unwrap(),
                physical_incarnation: "incarnation-1".into(),
                owned_paths: vec!["/workspace/a".into(), "/workspace/b".into()],
            },
        );
        assert!(
            !current.owned_paths_are_monotonic_to(&different_effect),
            "R3 effect fence"
        );

        let missing = SandboxHandle::local_v2(
            "sandbox",
            LocalSandboxHandleV2 {
                previous: current.local_payload().unwrap().clone(),
                realization_fingerprint: current.realization_fingerprint().unwrap().clone(),
                effect_fence: fence,
                physical_incarnation: "incarnation-1".into(),
                owned_paths: Vec::new(),
            },
        );
        assert!(!current.owned_paths_are_monotonic_to(&missing), "R4");
    }

    #[test]
    fn adoption_layout_unions_current_and_historical_owned_trees() {
        // Adoption-layout rules: A1 current and historical trees both disjoint
        // from Repository => admit; A2 the V2 evidence repeats the exact current
        // Repository tree => admit as one owner; A3 historical child overlaps
        // current Repo => reject; A4 a replacement Repo is nested under the
        // historically realized Repo tree => reject. A3/A4 model crash windows
        // where logical and physical layouts have not advanced atomically.
        let mut current = spec();
        current.mounts.push(MountRequirement {
            mount_id: "current-extra".into(),
            source: crate::vocab::MountSource::InlineBytes {
                contents: Vec::new(),
                content_hash: None,
            },
            mount_path: "/workspace/current-extra".into(),
            access: MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: true,
        });
        validate_repository_sandbox_adoption_layout(
            &["/workspace/repo"],
            &current,
            &["/workspace/historical-extra"],
        )
        .expect("A1");
        validate_repository_sandbox_adoption_layout(
            &["/workspace/repo"],
            &current,
            &["/workspace/repo"],
        )
        .expect("A2 exact current tree is not a competing owner");
        assert!(
            validate_repository_sandbox_adoption_layout(
                &["/workspace/repo"],
                &current,
                &["/workspace/repo/cache"],
            )
            .is_err(),
            "A3"
        );
        assert!(
            validate_repository_sandbox_adoption_layout(
                &["/workspace/repo/replacement"],
                &current,
                &["/workspace/repo"],
            )
            .is_err(),
            "A4"
        );
    }

    #[test]
    fn sandbox_effect_fence_validation_and_expiry_table_is_total() {
        // Cause/effect table: C1 operation/owner/runtime-incarnation are each
        // nonblank/blank; C2 now is before/at/after expiry; C3 a successor is
        // exact-lease with equal/longer/shorter expiry, higher epoch, or
        // same-epoch foreign. R1 all nonblank
        // fields construct one lossless neutral fence; R2 any blank identity is
        // rejected; R3 before expiry is live and at/after expiry is stale; R4
        // equal/longer same-lease and higher-epoch successors are authorized;
        // R5 shorter-expiry, same-epoch foreign, and older-epoch successors are
        // rejected. C4 the operation is exact/foreign; R6 the effect-scoped
        // predicate admits only R4 successors whose operation is exact. This
        // predicate is the sole provider successor authority.
        let fence =
            SandboxEffectFence::new("effect-1", "owner-1", "runtime-1", 7, 100).expect("R1");
        assert_eq!(fence.epoch, 7, "R1");
        assert!(!fence.expired_at(99), "R3 before");
        assert!(fence.expired_at(100), "R3 at");
        assert!(fence.expired_at(101), "R3 after");
        for (operation, owner, runtime) in [
            ("", "owner-1", "runtime-1"),
            ("effect-1", " ", "runtime-1"),
            ("effect-1", "owner-1", "\n"),
        ] {
            assert!(
                SandboxEffectFence::new(operation, owner, runtime, 7, 100).is_err(),
                "R2",
            );
        }
        for (rule, successor, expected) in [
            (
                "R4 equal expiry",
                SandboxEffectFence::new("next", "owner-1", "runtime-1", 7, 100).unwrap(),
                true,
            ),
            (
                "R4 longer expiry",
                SandboxEffectFence::new("next", "owner-1", "runtime-1", 7, 101).unwrap(),
                true,
            ),
            (
                "R4 higher epoch",
                SandboxEffectFence::new("next", "owner-2", "runtime-2", 8, 1).unwrap(),
                true,
            ),
            (
                "R5 shorter expiry",
                SandboxEffectFence::new("next", "owner-1", "runtime-1", 7, 99).unwrap(),
                false,
            ),
            (
                "R5 foreign same epoch",
                SandboxEffectFence::new("next", "owner-2", "runtime-2", 7, 101).unwrap(),
                false,
            ),
            (
                "R5 older epoch",
                SandboxEffectFence::new("next", "owner-1", "runtime-1", 6, 101).unwrap(),
                false,
            ),
        ] {
            assert_eq!(fence.authorizes_successor(&successor), expected, "{rule}");
        }
        let exact_renewal =
            SandboxEffectFence::new("effect-1", "owner-1", "runtime-1", 7, 101).unwrap();
        let foreign_operation =
            SandboxEffectFence::new("effect-2", "owner-1", "runtime-1", 7, 101).unwrap();
        assert!(
            fence.authorizes_effect_successor(&exact_renewal),
            "R6 exact operation"
        );
        assert!(
            !fence.authorizes_effect_successor(&foreign_operation),
            "R6 foreign operation"
        );
    }

    #[test]
    fn secret_writeback_identity_is_physical_while_authorization_is_current() {
        // Cause/effect graph: C1 the exact credential reference is same/different;
        // C2 the immutable physical Sandbox incarnation is same/different; C3 the
        // current aggregate authorization operation is predecessor/successor;
        // C4 reference/incarnation/fence identity is valid/invalid. Effects: E1
        // C1+C2 exact yields one stable writeback id across C3, so a terminal
        // successor can recognize a continuation response-loss replay; E2 a
        // different reference or physical incarnation yields a different id; E3
        // malformed identity is rejected before a broker or credential mutation.
        //
        // | Rule | reference | physical | authorization | Effect |
        // |---|---|---|---|---|
        // | S1 | exact | exact | predecessor | stable id A |
        // | S2 | exact | exact | successor | same id A / current fence B |
        // | S3 | different | exact | successor | different id |
        // | S4 | exact | different | successor | different id |
        // | S5 | blank | any | any | reject E3 |
        let predecessor = SandboxEffectFence::new(
            "continuation-secret-preparation",
            "worker-owner",
            "runtime-incarnation",
            7,
            u64::MAX,
        )
        .unwrap();
        let successor = SandboxEffectFence::new(
            "terminal-secret-preparation",
            "worker-owner",
            "runtime-incarnation",
            7,
            u64::MAX,
        )
        .unwrap();
        let first = SecretWritebackEffect::new("credential-source@3", "pod-uid-a", predecessor)
            .expect("S1");
        let replay =
            SecretWritebackEffect::new("credential-source@3", "pod-uid-a", successor.clone())
                .expect("S2");
        assert_eq!(first.writeback_id(), replay.writeback_id(), "S1/S2 E1");
        assert_eq!(
            replay.authorization(),
            &successor,
            "S2 current authorization"
        );
        assert_ne!(
            replay.writeback_id(),
            SecretWritebackEffect::new(
                "other-credential-source@3",
                "pod-uid-a",
                successor.clone(),
            )
            .unwrap()
            .writeback_id(),
            "S3/E2",
        );
        assert_ne!(
            replay.writeback_id(),
            SecretWritebackEffect::new("credential-source@3", "pod-uid-b", successor.clone(),)
                .unwrap()
                .writeback_id(),
            "S4/E2",
        );
        assert!(
            SecretWritebackEffect::new(" ", "pod-uid-a", successor.clone()).is_err(),
            "S5 blank reference",
        );
        let invalid_authorization = SandboxEffectFence {
            operation_id: " ".into(),
            owner: successor.owner.clone(),
            runtime_incarnation: successor.runtime_incarnation.clone(),
            epoch: successor.epoch,
            expires_at_unix_ms: successor.expires_at_unix_ms,
        };
        assert!(
            SecretWritebackEffect::new("credential-source@3", "pod-uid-a", invalid_authorization,)
                .is_err(),
            "S5 invalid authorization identity",
        );
        assert!(
            SecretWritebackEffect::new("credential-source@3", "\n", successor).is_err(),
            "S5 blank physical incarnation",
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
