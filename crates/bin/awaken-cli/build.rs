use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir
        .ancestors()
        .nth(3)
        .expect("awaken-cli must live under crates/bin/awaken-cli");
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

    build_console(&web);
    generate_embedded_assets(&web.join("dist"));
}

fn build_console(web: &Path) {
    let package_manager = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    if !web.join("node_modules").is_dir() {
        run(package_manager, web, &["install", "--frozen-lockfile"]);
    }
    run(package_manager, web, &["build"]);
}

fn run(program: &str, current_dir: &Path, args: &[&str]) {
    let status = Command::new(program)
        .current_dir(current_dir)
        .args(args)
        .status()
        .unwrap_or_else(|error| panic!("start {program}: {error}"));
    assert!(
        status.success(),
        "{program} {} exited with {status}",
        args.join(" ")
    );
}

fn generate_embedded_assets(dist: &Path) {
    assert!(
        dist.join("index.html").is_file(),
        "frontend build did not produce {}",
        dist.join("index.html").display()
    );

    let mut files = Vec::new();
    collect_files(dist, dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut generated = String::from(
        "pub(crate) fn embedded_asset(path: &str) -> Option<&'static [u8]> {\n\
         \x20   match path {\n",
    );
    for (relative, absolute) in files {
        generated.push_str(&format!(
            "        {:?} => Some(include_bytes!({:?})),\n",
            relative,
            absolute.to_string_lossy()
        ));
    }
    generated.push_str("        _ => None,\n    }\n}\n");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out_dir.join("embedded_console.rs"), generated)
        .expect("write embedded console asset table");
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read frontend distribution {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("read frontend distribution entry");
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("frontend asset must be under dist")
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
}
