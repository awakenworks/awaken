use std::env;
use std::path::PathBuf;

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
        .expect("awaken-process-startup must live under crates/apps/awaken-process-startup");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let graph = build_support::console_workspace_command(cfg!(windows), workspace)
        .command()
        .output()
        .expect("query Console workspace dependencies");
    assert!(
        graph.status.success(),
        "pnpm workspace graph failed: {}",
        String::from_utf8_lossy(&graph.stderr)
    );
    let inputs = build_support::console_source_inputs(workspace, &graph.stdout);
    for input in &inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let staged = build_support::stage_console_workspace(workspace, &out_dir, &inputs);
    let dist = staged.join("console");
    let [install, build] = build_support::console_commands(cfg!(windows), &staged, &dist);
    run(&install);
    run(&build);
    build_support::write_console_manifest(&staged, &dist);
    build_support::generate_embedded_assets(&dist, &out_dir);
}

fn run(spec: &build_support::ConsoleCommand) {
    let status = spec
        .command()
        .status()
        .unwrap_or_else(|error| panic!("start {}: {error}", spec.program));
    assert!(
        status.success(),
        "{} {:?} exited with {status}",
        spec.program,
        spec.args
    );
}
