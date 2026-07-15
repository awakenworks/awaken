//! Environment-variable injection into a real sandboxed process, across both local
//! tiers (Workdir + Namespace/bwrap). Complements the ACP-CLI env projection tests
//! (`acp_cli.rs`, the model/base_url/secret precedence) with the *other* env altitude:
//! the provisioning contract's [`pc::EnvVar`] injected into the process the sandbox runs.
//!
//! Invariants proven here:
//! - an `Inline` var is visible in the process (both tiers);
//! - a `Secret` value is NEVER injected as plaintext (the provider only realizes
//!   `Inline`; a broker reference must not leak into the process address space — G3);
//! - the runtime-owned dirs (`AWAKEN_OUTPUTS_DIR`) and user vars coexist.
//!
//! bwrap exec self-skips when userns is unavailable; the Workdir tier always runs.

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

#[derive(Clone, Copy, Debug)]
enum Tier {
    Workdir,
    Namespace,
}

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

fn spec(scope: &str, isolation: pc::IsolationClass) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn provider(tier: Tier, base: &std::path::Path) -> Box<dyn pc::SandboxProvider> {
    match tier {
        Tier::Workdir => Box::new(LocalProvider::new(base)),
        Tier::Namespace => Box::new(NamespaceProvider::new(base)),
    }
}

fn isolation(tier: Tier) -> pc::IsolationClass {
    match tier {
        Tier::Workdir => pc::IsolationClass::Workdir,
        Tier::Namespace => pc::IsolationClass::Namespace,
    }
}

fn out(tier: Tier, name: &str) -> String {
    match tier {
        // The Workdir jail addresses outputs via the injected env var; the Namespace
        // tier has the real absolute path.
        Tier::Workdir => format!("$AWAKEN_OUTPUTS_DIR/{name}"),
        Tier::Namespace => format!("/mnt/session/outputs/{name}"),
    }
}

fn sh(script: String) -> pc::Command {
    let mut c = pc::Command::new(["sh", "-c", script.as_str()]);
    c.stdio = pc::Stdio::Null;
    c
}

async fn read_only_artifact(sandbox: &dyn pc::Sandbox, suffix: &str) -> Vec<u8> {
    let arts = sandbox.artifacts().await.unwrap();
    let a = arts
        .iter()
        .find(|a| a.path.ends_with(suffix))
        .unwrap_or_else(|| panic!("artifact {suffix} written"));
    sandbox.read_artifact(&a.id).await.unwrap()
}

/// Run one env-injection scenario against a tier; returns false when self-skipped.
async fn run(tier: Tier, can_exec: bool) -> bool {
    if !can_exec {
        return false;
    }
    let tmp = tempfile::tempdir().unwrap();
    let p = provider(tier, tmp.path());
    let mut s = spec("t-envinj", isolation(tier));

    // A plain user var (Inline/Process) and a secret var (a broker reference).
    s.env.push(pc::EnvVar {
        name: "GREETING".into(),
        value: pc::EnvValue::Inline {
            value: "salut".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    s.env.push(pc::EnvVar {
        name: "MYSECRET".into(),
        value: pc::EnvValue::Secret {
            reference: "broker://k".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });

    let sandbox = p
        .create(&s)
        .await
        .unwrap_or_else(|e| panic!("{tier:?}: create with env: {e:?}"));

    // Inline is visible; the secret reference is NOT injected as plaintext; the
    // runtime-owned outputs dir is present (we just wrote through it).
    let script = format!(
        "printf '%s|%s' \"$GREETING\" \"$MYSECRET\" > {}",
        out(tier, "env.txt")
    );
    let proc = sandbox.spawn(sh(script)).await.unwrap();
    assert_eq!(
        proc.wait().await.unwrap().code,
        Some(0),
        "{tier:?}: process exits 0"
    );

    let got = read_only_artifact(&*sandbox, "/env.txt").await;
    let got = String::from_utf8(got).unwrap();
    let (inline, secret) = got.split_once('|').unwrap();
    assert_eq!(
        inline, "salut",
        "{tier:?}: Inline env var is visible in the process"
    );
    assert_eq!(
        secret, "",
        "{tier:?}: a Secret env reference must NEVER be injected as plaintext (got {secret:?})"
    );

    sandbox.dispose().await.unwrap();
    true
}

#[tokio::test]
async fn workdir_injects_inline_and_never_leaks_a_secret_reference() {
    assert!(run(Tier::Workdir, true).await, "Workdir always execs");
}

#[tokio::test]
async fn namespace_injects_inline_and_never_leaks_a_secret_reference() {
    let bwrap = bwrap_works().await;
    if !run(Tier::Namespace, bwrap).await {
        eprintln!("skipping: bwrap/userns unavailable — Namespace env-injection exec self-skipped");
    }
}

/// The reserved runtime-owned keys are rejected at admission before any provisioning —
/// a declaration may not shadow `AWAKEN_OUTPUTS_DIR`/`PATH`/… (the runtime owns them).
#[test]
fn reserved_runtime_env_keys_are_rejected_at_admission() {
    for reserved in pc::RESERVED_ENV_KEYS {
        let decl = pc::EnvironmentDecl {
            summary: "env admission".into(),
            kind: "sandbox".into(),
            required_field: None,
            writable_base: false,
            max_concurrency: Some(1),
            env_keys: vec![(*reserved).to_string()],
        };
        assert!(
            pc::check_environment_soundness(&decl).is_err(),
            "reserved key {reserved} must be rejected at admission"
        );
    }
}
