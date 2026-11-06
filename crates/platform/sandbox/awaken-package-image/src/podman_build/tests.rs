//! Behavior tests for the Podman package-image builder: content-addressed
//! builds, cache hits, registry repair, the build network and the deadline.

use std::sync::{Arc, Mutex};

use awaken_container_contract::PackageImageProvisioner;
use awaken_local_process::podman::CmdOutput;

use super::*;

/// A scripted [`CommandExec`]: a handler maps `args` to a canned output, and
/// every invocation's argv is recorded so tests can assert what was run.
type CommandHandler = dyn Fn(&[String]) -> CmdOutput + Send + Sync;

struct FakeExec {
    handler: Box<CommandHandler>,
    calls: Mutex<Vec<Vec<String>>>,
}

struct BlockingExec;

#[async_trait]
impl CommandExec for BlockingExec {
    async fn exec(&self, _bin: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        std::future::pending().await
    }
}

#[async_trait]
impl CommandExec for FakeExec {
    async fn exec(&self, _bin: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        Ok((self.handler)(args))
    }
}

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        ok: true,
        status_code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn err(stderr: &str) -> CmdOutput {
    CmdOutput {
        ok: false,
        status_code: Some(125),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// Build a provisioner whose executor replies per `handler`, plus a handle to
/// the recorded argv list.
fn provisioner_with(
    handler: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static,
) -> (PodmanPackageImageProvisioner, Arc<FakeExec>) {
    let fake = Arc::new(FakeExec {
        handler: Box::new(handler),
        calls: Mutex::new(Vec::new()),
    });
    (
        PodmanPackageImageProvisioner::with_exec("podman", fake.clone()),
        fake,
    )
}

#[tokio::test]
async fn package_preparation_has_a_fail_closed_deadline() {
    let runtime = PodmanPackageImageProvisioner::with_exec("podman", Arc::new(BlockingExec))
        .with_package_build_timeout(std::time::Duration::from_millis(10));
    let requirements = awaken_environment_contract::PackageRequirements {
        managers: [("npm".to_owned(), vec!["cowsay@1.6.0".to_owned()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let error = PackageImageProvisioner::prepare_package_image(
        &runtime,
        "base:1",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect_err("a stuck package resolver must not activate the base image");
    assert!(
        error.to_string().contains("exceeded its deadline"),
        "{error}"
    );
}

#[test]
fn registry_auth_file_is_scoped_to_pull_and_push() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let rt = PodmanPackageImageProvisioner::with_exec(
        "podman",
        Arc::new(FakeExec {
            handler: Box::new(|_| ok("")),
            calls: Mutex::new(Vec::new()),
        }),
    )
    .with_package_registry("registry.internal")
    .with_package_registry_auth_file(file.path())
    .unwrap();
    let pull = rt.registry_command("pull", "registry.internal/awaken-packages@sha256:abc");
    assert_eq!(pull[0], "pull");
    assert_eq!(pull[1], "--authfile");
    assert_eq!(pull[2], file.path().to_string_lossy());
    assert_eq!(pull[3], "registry.internal/awaken-packages@sha256:abc");
}

#[tokio::test]
async fn registry_mode_repairs_a_missing_remote_from_the_local_cache() {
    let (rt, fake) = provisioner_with(|args| match args.first().map(String::as_str) {
        Some("pull") => err("manifest unknown"),
        Some("push") => ok(""),
        Some("image") if args.get(1).map(String::as_str) == Some("exists") => ok(""),
        Some("image") if args.iter().any(|arg| arg.contains("RepoDigests")) => {
            ok("registry.internal/awaken-packages@sha256:remote")
        }
        Some("image") => ok("sha256:exact-base"),
        other => panic!("unexpected podman command: {other:?}"),
    });
    let rt = rt.with_package_registry("registry.internal");
    let requirements = awaken_environment_contract::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let image = PackageImageProvisioner::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .unwrap();
    assert_eq!(image, "registry.internal/awaken-packages@sha256:remote");
    let calls = fake.calls.lock().unwrap();
    let pull = calls
        .iter()
        .position(|args| args.first().map(String::as_str) == Some("pull"))
        .unwrap();
    let push = calls
        .iter()
        .position(|args| args.first().map(String::as_str) == Some("push"))
        .unwrap();
    assert!(pull < push, "remote probe must precede repair push");
    assert!(
        !calls
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("build")),
        "a deterministic local hit repairs the registry without rebuilding"
    );
}

/// Podman package-image cause graph:
/// mutable base reference -> exact local image ID; exact ID + exact package
/// requirements -> content-addressed Containerfile/tag -> cache probe.
/// Cache miss builds exactly once; cache hit performs no build. A workload
/// container is never created by this operation.
///
/// | Rule | base inspect | cache | observable behavior |
/// |---|---|---|---|
/// | P1 | exact ID | miss | build once FROM exact ID; return derived ref |
/// | P2 | exact ID | hit | return derived ref without a build |
/// | P3 | missing/empty | n/a | fail before cache probe/build |
#[tokio::test]
async fn package_requirements_build_one_content_addressed_image_on_cache_miss() {
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_build = captured.clone();
    let (rt, fake) = provisioner_with(move |args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
            (Some("image"), Some("exists")) => err("not found"),
            (Some("build"), _) => {
                let file = args
                    .iter()
                    .position(|arg| arg == "--file")
                    .and_then(|index| args.get(index + 1))
                    .expect("build carries Containerfile");
                *captured_build.lock().unwrap() =
                    Some(std::fs::read_to_string(file).expect("Containerfile exists during build"));
                ok("")
            }
            other => panic!("unexpected podman command: {other:?}"),
        }
    });
    let requirements = awaken_environment_contract::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let image = PackageImageProvisioner::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect("cache miss builds");
    assert!(image.starts_with("localhost/awaken-packages:"));
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 3, "P1: inspect, cache probe, then build");
    assert_eq!(&calls[0][..2], &["image", "inspect"]);
    assert_eq!(&calls[1][..2], &["image", "exists"]);
    assert_eq!(calls[2].first().map(String::as_str), Some("build"));
    let file = captured.lock().unwrap().clone().unwrap();
    assert!(file.starts_with("FROM sha256:exact-base\n"), "P1: {file}");
    assert!(
        file.contains(r#"RUN ["/usr/bin/env","pip","install","--no-cache-dir","httpx==0.28.0"]"#)
    );
}

#[tokio::test]
async fn package_image_cache_hit_does_not_build_or_create_a_workload() {
    let (rt, fake) = provisioner_with(|args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
            (Some("image"), Some("exists")) => ok(""),
            other => panic!("P2 forbids build/run calls: {other:?}"),
        }
    });
    let requirements = awaken_environment_contract::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let image = PackageImageProvisioner::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect("P2 cache hit");

    assert!(image.starts_with("localhost/awaken-packages:"));
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "P2: inspect and cache probe only");
    assert_eq!(&calls[0][..2], &["image", "inspect"]);
    assert_eq!(&calls[1][..2], &["image", "exists"]);
}

#[tokio::test]
async fn missing_base_identity_fails_before_cache_or_build_side_effects() {
    let (rt, fake) = provisioner_with(|args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok(""),
            other => panic!("P3 forbids cache/build calls: {other:?}"),
        }
    });
    let requirements = awaken_environment_contract::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let error = PackageImageProvisioner::prepare_package_image(
        &rt,
        "missing:latest",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect_err("P3 empty identity fails closed");

    assert!(error.to_string().contains("empty base-image identity"));
    assert_eq!(fake.calls.lock().unwrap().len(), 1, "P3: inspect only");
}

#[tokio::test]
async fn package_build_network_reaches_the_podman_build_argv() {
    // The same cause/effect table Docker's `NetworkMode` selection obeys, read
    // off the real `podman build` argv. B1 Unrestricted without the opt-in ->
    // the engine's own build network, so no `--network` flag at all;
    // B2 Unrestricted with the opt-in -> `--network host`; B3 None -> always
    // `--network none`, opt-in or not; B4 Allowlist -> refused before any
    // build. The knob existed on Docker alone until the maximal-feature gate
    // compiled this tier, so the deployment setting silently did nothing here.
    async fn build_argv(network: pc::NetworkPolicy, host_opt_in: bool) -> Option<Vec<String>> {
        let (rt, fake) = provisioner_with(|args| match args.first().map(String::as_str) {
            Some("image") if args.get(1).map(String::as_str) == Some("exists") => {
                err("no such image")
            }
            Some("image") => ok("sha256:exact-base"),
            Some("build") => ok(""),
            other => panic!("unexpected podman command: {other:?}"),
        });
        let rt = rt.with_package_build_host_network(host_opt_in);
        let requirements = awaken_environment_contract::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let outcome = PackageImageProvisioner::prepare_package_image(
            &rt,
            "python:3.13",
            &requirements,
            &network,
        )
        .await;
        if matches!(network, pc::NetworkPolicy::Allowlist { .. }) {
            let error = outcome.expect_err("B4 an allowlist has no no-bypass build network");
            assert!(
                error.to_string().contains("no-bypass allowlist network"),
                "B4 states why it refused: {error}"
            );
        } else {
            outcome.expect("B1-B3 build the package image");
        }
        let calls = fake.calls.lock().unwrap();
        calls
            .iter()
            .find(|args| args.first().map(String::as_str) == Some("build"))
            .cloned()
    }

    fn network_flag(argv: &[String]) -> Option<&str> {
        argv.iter()
            .position(|arg| arg == "--network")
            .and_then(|index| argv.get(index + 1))
            .map(String::as_str)
    }

    let b1 = build_argv(pc::NetworkPolicy::Unrestricted, false)
        .await
        .expect("B1 builds");
    assert_eq!(
        network_flag(&b1),
        None,
        "B1 keeps the engine's build network"
    );
    let b2 = build_argv(pc::NetworkPolicy::Unrestricted, true)
        .await
        .expect("B2 builds");
    assert_eq!(
        network_flag(&b2),
        Some("host"),
        "B2 opts in to the host network"
    );
    for opt_in in [false, true] {
        let b3 = build_argv(pc::NetworkPolicy::None, opt_in)
            .await
            .expect("B3 builds");
        assert_eq!(
            network_flag(&b3),
            Some("none"),
            "B3 a denied Environment denies its build too, opt-in {opt_in}"
        );
    }
    for opt_in in [false, true] {
        assert!(
            build_argv(
                pc::NetworkPolicy::Allowlist {
                    hosts: vec!["pypi.org".into()],
                },
                opt_in,
            )
            .await
            .is_none(),
            "B4 refuses before any build, opt-in {opt_in}"
        );
    }

    let rt = PodmanPackageImageProvisioner::with_exec(
        "podman",
        Arc::new(FakeExec {
            handler: Box::new(|_| ok("")),
            calls: Mutex::new(Vec::new()),
        }),
    );
    assert!(
        !rt.package_build_host_network,
        "the opt-in is off by default"
    );
    assert!(
        rt.with_package_build_host_network(true)
            .package_build_host_network,
        "the deployment selects it explicitly"
    );
}
