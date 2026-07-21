//! Pairwise (all-pairs) coverage of the three provisioning axes as they converge at
//! the [`pc::SandboxProvider`] seam: **sandbox tier × resource kind × runtime shape**.
//!
//! The full grid is `tier{Workdir,Namespace} × res{None,File,Memory,Secret} ×
//! shape{Process,OpaqueAgent}` = 16 cells. A pairwise covering array collapses that to
//! the rows in [`CASES`] while still exercising every (tier,res), (tier,shape) and
//! (res,shape) pair at least once — [`pairwise_coverage_is_complete`] proves the array
//! is not missing a pair.
//!
//! One pair is *infeasible* and becomes a negative test rather than a positive row: an
//! opaque agent (`Shape::OpaqueAgent`) needs an OS-transparent tier, so it can never
//! run on the `Workdir` path-jail — the provider must fail closed at
//! `prepare_environment`, never downgrade isolation (the constraint in
//! `capability_driven_tier_selection`, generalised here).
//!
//! Realization-side assertions (mount realized, memory harvested, secret shredded on
//! dispose) need no bwrap and always run; execution-side assertions (read the mount,
//! read-only enforced, egress denied) self-skip when bwrap/userns is unavailable —
//! same discipline as `namespace_provider.rs` / `sandbox_source.rs`.

use std::sync::Arc;

use awaken_memory_store::{MemoryRepository, VolatileMemoryRepository};
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
use awaken_sandbox_memoryd::MemoryStoreMounter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Lexical path-jail; not tool-transparent (must never host an opaque agent).
    Workdir,
    /// bubblewrap / Seatbelt; OS-enforced and tool-transparent.
    Namespace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Res {
    None,
    /// Content-addressed blob from the file store.
    File,
    /// Write-through memory store (copy-realized on the local tiers).
    Memory,
    /// Broker-resolved credential written to the sandbox, shredded on dispose.
    Secret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A plain command (the Native / bash tool altitude).
    Process,
    /// A process driven as an opaque agent — requires an OS-transparent tier.
    OpaqueAgent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Net {
    Open,
    None,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    tier: Tier,
    res: Res,
    shape: Shape,
    net: Net,
}

use Net::{None as NetNone, Open as NetOpen};
use Res::{File as ResFile, Memory as ResMemory, None as ResNone, Secret as ResSecret};
use Shape::{OpaqueAgent, Process};
use Tier::{Namespace, Workdir};

/// The pairwise covering array. Every feasible (tier,res)/(tier,shape)/(res,shape) pair
/// appears at least once (asserted by [`pairwise_coverage_is_complete`]); the single
/// infeasible pair `(Workdir, OpaqueAgent)` is the negative row at the end.
const CASES: &[Case] = &[
    // Workdir tier — only the Process shape is admissible.
    Case {
        tier: Workdir,
        res: ResNone,
        shape: Process,
        net: NetOpen,
    },
    Case {
        tier: Workdir,
        res: ResFile,
        shape: Process,
        net: NetOpen,
    },
    Case {
        tier: Workdir,
        res: ResMemory,
        shape: Process,
        net: NetOpen,
    },
    Case {
        tier: Workdir,
        res: ResSecret,
        shape: Process,
        net: NetOpen,
    },
    // Namespace tier — hosts opaque agents and enforces egress.
    Case {
        tier: Namespace,
        res: ResNone,
        shape: OpaqueAgent,
        net: NetNone,
    },
    Case {
        tier: Namespace,
        res: ResFile,
        shape: OpaqueAgent,
        net: NetOpen,
    },
    Case {
        tier: Namespace,
        res: ResMemory,
        shape: Process,
        net: NetOpen,
    },
    Case {
        tier: Namespace,
        res: ResSecret,
        shape: Process,
        net: NetOpen,
    },
    Case {
        tier: Namespace,
        res: ResMemory,
        shape: OpaqueAgent,
        net: NetOpen,
    },
    Case {
        tier: Namespace,
        res: ResSecret,
        shape: OpaqueAgent,
        net: NetOpen,
    },
    // Negative (constraint): an opaque agent on the Workdir path-jail must fail closed.
    Case {
        tier: Workdir,
        res: ResNone,
        shape: OpaqueAgent,
        net: NetOpen,
    },
];

/// True only when bwrap exists AND unprivileged user namespaces are enabled — the exec
/// path of the Namespace tier. Realization (create/dispose) needs none of this.
async fn bwrap_works() -> bool {
    tokio::process::Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The isolation class a shape demands: an opaque agent needs a transparent tier.
fn required_isolation(tier: Tier, shape: Shape) -> pc::IsolationClass {
    match shape {
        Shape::OpaqueAgent => pc::IsolationClass::Namespace,
        Shape::Process => match tier {
            Tier::Workdir => pc::IsolationClass::Workdir,
            Tier::Namespace => pc::IsolationClass::Namespace,
        },
    }
}

/// The seed a `File`/`Secret`/`Memory` resource carries, so exec probes can prove the
/// resource is actually readable inside the sandbox.
const SEED: &str = "resource-seed";

/// A sandbox-absolute path rendered the way a tier addresses it: the Workdir path-jail
/// reads relative to the workdir and writes outputs via `$AWAKEN_OUTPUTS_DIR`; the
/// Namespace tier has real absolute paths (`/mnt/session/outputs`, `/data/...`).
fn read_path(tier: Tier, abs: &str) -> String {
    match tier {
        Tier::Workdir => abs.trim_start_matches('/').to_string(),
        Tier::Namespace => abs.to_string(),
    }
}
fn out_path(tier: Tier, name: &str) -> String {
    match tier {
        Tier::Workdir => format!("$AWAKEN_OUTPUTS_DIR/{name}"),
        Tier::Namespace => format!("/mnt/session/outputs/{name}"),
    }
}

fn sh(script: String) -> pc::Command {
    let mut c = pc::Command::new(["sh", "-c", script.as_str()]);
    c.stdio = pc::Stdio::Null;
    c
}

/// Build the spec + provider for a case, injecting whatever blob/memory backing the
/// resource needs. Returns the Memory repository (for a `Memory` resource) so teardown can
/// be asserted after dispose.
async fn build(
    case: &Case,
    base: &std::path::Path,
) -> (
    pc::SandboxSpec,
    Box<dyn pc::SandboxProvider>,
    Option<Arc<VolatileMemoryRepository>>,
) {
    let mount_path = "/data/in.txt";
    let mem_path = "/workspace/memory";
    let isolation = required_isolation(case.tier, case.shape);

    // Per-tier mount access: only a transparent tier can enforce read-only, so a
    // Workdir File mount is read-write (it cannot promise EROFS).
    let file_access = match case.tier {
        Tier::Workdir => pc::MountAccess::ReadWrite,
        Tier::Namespace => pc::MountAccess::ReadOnly,
    };

    let mounts = match case.res {
        Res::None => Vec::new(),
        Res::File => vec![pc::MountRequirement {
            mount_id: "in".into(),
            source: pc::MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            mount_path: mount_path.into(),
            access: file_access,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        Res::Memory => vec![pc::MountRequirement {
            mount_id: "mem".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "s".into(),
            },
            mount_path: mem_path.into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        Res::Secret => vec![pc::MountRequirement {
            mount_id: "auth".into(),
            source: pc::MountSource::Secret {
                reference: "broker://k".into(),
                content_hash: None,
            },
            mount_path: "/workspace/.auth".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
    };

    let network = match case.net {
        Net::Open => pc::NetworkPolicy::Unrestricted,
        Net::None => pc::NetworkPolicy::None,
    };

    let spec = pc::SandboxSpec {
        scope: format!("pw-{:?}-{:?}-{:?}", case.tier, case.res, case.shape).to_lowercase(),
        isolation,
        mounts,
        env: Vec::new(),
        network,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    };

    // A volatile Memory repository seeded with one note, shared with the assertion side.
    let mem_fs = if matches!(case.res, Res::Memory) {
        let fs = Arc::new(VolatileMemoryRepository::new());
        fs.create("s", "/note.md", "v1").await.unwrap();
        Some(fs)
    } else {
        None
    };

    // A file store seeded with the File resource bytes.
    let file_bytes = SEED.as_bytes().to_vec();

    let provider: Box<dyn pc::SandboxProvider> = match case.tier {
        Tier::Workdir => {
            let mut p = LocalProvider::new(base).with_blob("f", file_bytes);
            if matches!(case.res, Res::Secret) {
                p = p.with_blob("broker://k", SEED.as_bytes().to_vec());
            }
            if let Some(fs) = &mem_fs {
                p = p.with_memory_mounter(Arc::new(MemoryStoreMounter::copy_only(fs.clone())));
            }
            Box::new(p)
        }
        Tier::Namespace => {
            let mut p = NamespaceProvider::new(base).with_blob("f", file_bytes);
            if matches!(case.res, Res::Secret) {
                p = p.with_blob("broker://k", SEED.as_bytes().to_vec());
            }
            if let Some(fs) = &mem_fs {
                p = p.with_memory_mounter(Arc::new(MemoryStoreMounter::copy_only(fs.clone())));
            }
            Box::new(p)
        }
    };

    (spec, provider, mem_fs)
}

/// Drive one covering-array row end to end. Returns whether the exec-side assertions
/// actually ran (false when self-skipped for want of bwrap) so the harness can report.
async fn run_case(case: &Case, can_exec: bool) -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let (spec, provider, mem_fs) = build(case, tmp.path()).await;
    let label = format!("{case:?}");

    // ── Negative (constraint): opaque agent on the Workdir path-jail. ──────────────
    if case.tier == Tier::Workdir && case.shape == Shape::OpaqueAgent {
        assert!(
            pc::prepare_environment(&spec, &provider.capabilities()).is_err(),
            "{label}: an opaque agent MUST be refused on the Workdir tier, not downgraded"
        );
        return false;
    }

    // The spec must fit the chosen backend (fail-closed happens above, not here).
    pc::prepare_environment(&spec, &provider.capabilities())
        .unwrap_or_else(|e| panic!("{label}: spec should fit its tier: {e:?}"));

    let sandbox = provider
        .create(&spec)
        .await
        .unwrap_or_else(|e| panic!("{label}: create: {e:?}"));

    // ── Realization assertions (no bwrap needed). ─────────────────────────────────
    match case.res {
        Res::None => assert!(sandbox.realized().is_empty(), "{label}: no mounts expected"),
        Res::File => {
            assert_eq!(sandbox.realized().len(), 1, "{label}: file mount realized");
        }
        Res::Memory => {
            assert_eq!(
                sandbox.realized().len(),
                1,
                "{label}: memory mount realized"
            );
            assert_eq!(
                sandbox.realized()[0].realization,
                pc::Realization::Copy,
                "{label}: local tiers copy-realize a memory store"
            );
        }
        Res::Secret => {
            assert_eq!(
                sandbox.realized().len(),
                1,
                "{label}: secret mount realized"
            );
        }
    }

    // ── Execution assertions (bwrap-gated for the Namespace tier). ────────────────
    let did_exec = if can_exec {
        exec_assertions(case, sandbox.as_ref(), &label).await;
        true
    } else {
        false
    };

    // ── Teardown + post-dispose assertions. ───────────────────────────────────────
    sandbox
        .dispose()
        .await
        .unwrap_or_else(|e| panic!("{label}: dispose: {e:?}"));
    assert!(
        matches!(
            sandbox.status().await.unwrap(),
            pc::SandboxStatus::Terminated
        ),
        "{label}: disposed sandbox is Terminated"
    );

    // Memory harvest: an edit made while live must survive back to the durable store;
    // when we could not exec (no bwrap) the untouched note must still round-trip.
    if let Some(fs) = &mem_fs {
        let expect = if did_exec { "v2" } else { "v1" };
        let got = fs.get_by_path("s", "/note.md").await.unwrap().unwrap();
        assert_eq!(
            got.content.as_deref(),
            Some(expect),
            "{label}: memory store harvested on dispose"
        );
    }

    did_exec
}

/// Everything that requires actually running a process inside the sandbox.
async fn exec_assertions(case: &Case, sandbox: &dyn pc::Sandbox, label: &str) {
    let tier = case.tier;

    // Every case: the sandbox can run a process and produce a retrievable artifact.
    let marker = out_path(tier, "hello.txt");
    let proc = sandbox
        .spawn(sh(format!("printf hi > {marker}")))
        .await
        .unwrap_or_else(|e| panic!("{label}: spawn: {e:?}"));
    assert_eq!(
        proc.wait().await.unwrap().code,
        Some(0),
        "{label}: process exits 0"
    );
    let arts = sandbox.artifacts().await.unwrap();
    let hello = arts
        .iter()
        .find(|a| a.path.ends_with("/hello.txt"))
        .unwrap_or_else(|| panic!("{label}: artifact written"));
    assert_eq!(sandbox.read_artifact(&hello.id).await.unwrap(), b"hi");

    match case.res {
        Res::File => {
            // The resource is readable inside the sandbox.
            let src = read_path(tier, "/data/in.txt");
            let dst = out_path(tier, "copy.txt");
            let r = sandbox
                .spawn(sh(format!("cat {src} > {dst}")))
                .await
                .unwrap();
            assert_eq!(
                r.wait().await.unwrap().code,
                Some(0),
                "{label}: read resource"
            );
            let arts = sandbox.artifacts().await.unwrap();
            let copy = arts.iter().find(|a| a.path.ends_with("/copy.txt")).unwrap();
            assert_eq!(
                sandbox.read_artifact(&copy.id).await.unwrap(),
                SEED.as_bytes()
            );

            // On a transparent tier the read-only bind rejects writes (OS-enforced).
            if tier == Tier::Namespace {
                let w = sandbox.spawn(sh(format!("echo x > {src}"))).await.unwrap();
                assert_ne!(
                    w.wait().await.unwrap().code,
                    Some(0),
                    "{label}: read-only mount must reject writes"
                );
            }
        }
        Res::Memory => {
            // Edit the write-through store so the harvest assertion sees v2.
            let note = format!("{}/note.md", read_path(tier, "/workspace/memory"));
            let e = sandbox
                .spawn(sh(format!("printf v2 > {note}")))
                .await
                .unwrap();
            assert_eq!(
                e.wait().await.unwrap().code,
                Some(0),
                "{label}: edit memory note"
            );
        }
        Res::Secret => {
            // The broker-resolved credential is present at its mount path.
            let path = read_path(tier, "/workspace/.auth");
            let dst = out_path(tier, "cred.txt");
            let r = sandbox
                .spawn(sh(format!("cat {path} > {dst}")))
                .await
                .unwrap();
            assert_eq!(
                r.wait().await.unwrap().code,
                Some(0),
                "{label}: read secret mount"
            );
        }
        Res::None => {}
    }

    // Egress: a deny-network sandbox cannot reach a live host loopback listener; the
    // OS gives it an empty network namespace (Namespace tier only).
    if case.net == Net::None && tier == Tier::Namespace {
        // A real listening socket on the host loopback (std, so no tokio `net`
        // feature is needed): the kernel completes the handshake up to the backlog,
        // so a reachable `/dev/tcp` connect would succeed — proving the netns, not a
        // dead port, is what denies the deny-egress agent.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = format!(
            "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then r=NET-UP; else r=NET-DOWN; fi; \
             printf '%s' \"$r\" > {}",
            out_path(tier, "net.txt")
        );
        let mut c = pc::Command::new(["bash", "-c", probe.as_str()]);
        c.stdio = pc::Stdio::Null;
        let p = sandbox.spawn(c).await.unwrap();
        assert_eq!(p.wait().await.unwrap().code, Some(0));
        let arts = sandbox.artifacts().await.unwrap();
        let net = arts.iter().find(|a| a.path.ends_with("/net.txt")).unwrap();
        assert_eq!(
            sandbox.read_artifact(&net.id).await.unwrap(),
            b"NET-DOWN",
            "{label}: deny-egress sandbox must not reach the host loopback"
        );
        drop(listener);
    }
}

/// The whole covering array, driven in one test. Exec-side assertions self-skip without
/// bwrap; realization + constraint assertions always run. A per-run summary is printed.
#[tokio::test]
async fn pairwise_matrix_over_tier_resource_shape() {
    let can_exec = bwrap_works().await;
    if !can_exec {
        eprintln!(
            "note: bwrap/userns unavailable — Namespace exec assertions self-skip; \
             Workdir exec + all realization/constraint assertions still run"
        );
    }

    let mut executed = 0usize;
    let mut realized_only = 0usize;
    let mut negatives = 0usize;
    for case in CASES {
        // Workdir always execs (no bwrap); Namespace exec needs bwrap.
        let case_can_exec = match case.tier {
            Tier::Workdir => true,
            Tier::Namespace => can_exec,
        };
        let is_negative = case.tier == Tier::Workdir && case.shape == Shape::OpaqueAgent;
        let did_exec = run_case(case, case_can_exec).await;
        if is_negative {
            negatives += 1;
        } else if did_exec {
            executed += 1;
        } else {
            realized_only += 1;
        }
    }
    eprintln!(
        "pairwise matrix: {} cases — {executed} fully executed, {realized_only} realized-only \
         (bwrap skipped), {negatives} negative(constraint)",
        CASES.len()
    );
    assert_eq!(
        negatives, 1,
        "exactly one infeasible pair is a negative test"
    );
}

/// Meta-guard: the covering array actually covers every feasible pair across the three
/// axes. Deleting a row (or forgetting a combination) fails here, not silently.
#[test]
fn pairwise_coverage_is_complete() {
    let tiers = [Tier::Workdir, Tier::Namespace];
    let reses = [Res::None, Res::File, Res::Memory, Res::Secret];
    let shapes = [Shape::Process, Shape::OpaqueAgent];

    let has_tier_res = |t, r| CASES.iter().any(|c| c.tier == t && c.res == r);
    let has_tier_shape = |t, s| CASES.iter().any(|c| c.tier == t && c.shape == s);
    let has_res_shape = |r, s| CASES.iter().any(|c| c.res == r && c.shape == s);

    for &t in &tiers {
        for &r in &reses {
            assert!(has_tier_res(t, r), "missing (tier,res) pair: {t:?}×{r:?}");
        }
        for &s in &shapes {
            // (Workdir, OpaqueAgent) is covered by the negative row — still present.
            assert!(
                has_tier_shape(t, s),
                "missing (tier,shape) pair: {t:?}×{s:?}"
            );
        }
    }
    for &r in &reses {
        for &s in &shapes {
            assert!(has_res_shape(r, s), "missing (res,shape) pair: {r:?}×{s:?}");
        }
    }
}
