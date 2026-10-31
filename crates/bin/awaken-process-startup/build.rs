use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

mod build_support;

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads the build environment Cargo sets"
)]
fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir
        .ancestors()
        .nth(3)
        .expect("awaken-process-startup must live under crates/bin/awaken-process-startup");
    let web = workspace.join("web");

    println!("cargo:rerun-if-changed={}", web.join("src").display());
    for input in [
        "index.html",
        "package.json",
        "tsconfig.json",
        "vite.config.ts",
    ] {
        println!("cargo:rerun-if-changed={}", web.join(input).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("pnpm-lock.yaml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("pnpm-workspace.yaml").display()
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dist = out_dir.join("console");
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("package.json").display()
    );
    build_console(workspace, &dist);
    build_support::write_console_manifest(workspace, &dist);
    build_support::generate_embedded_assets(&dist, &out_dir);
}

fn build_console(workspace: &Path, dist: &Path) {
    let [install, build] = build_support::console_commands(cfg!(windows), workspace, dist);
    run(&install);
    let workspace = command(&build_support::console_workspace_command(
        cfg!(windows),
        workspace,
    ))
    .output()
    .expect("query Console workspace dependencies");
    assert!(
        workspace.status.success(),
        "pnpm workspace graph failed: {}",
        String::from_utf8_lossy(&workspace.stderr)
    );
    for input in build_support::console_package_inputs(&workspace.stdout) {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    run(&build);
}

fn command(spec: &build_support::ConsoleCommand) -> Command {
    // Ambient E2E TS loaders must not affect pnpm's package resolution.
    let mut command = Command::new(spec.program);
    command
        .env_remove("NODE_OPTIONS")
        .current_dir(&spec.current_dir)
        .args(&spec.args);
    command
}

fn run(spec: &build_support::ConsoleCommand) {
    let status = command(spec)
        .status()
        .unwrap_or_else(|error| panic!("start {}: {error}", spec.program));
    assert!(
        status.success(),
        "{} {:?} exited with {status}",
        spec.program,
        spec.args
    );
}
