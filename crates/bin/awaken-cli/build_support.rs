use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConsoleCommand {
    pub(crate) program: &'static str,
    pub(crate) current_dir: PathBuf,
    pub(crate) args: Vec<OsString>,
}

fn pnpm(windows: bool) -> &'static str {
    if windows { "pnpm.cmd" } else { "pnpm" }
}

pub(crate) fn console_commands(
    windows: bool,
    workspace: &Path,
    dist: &Path,
) -> [ConsoleCommand; 2] {
    [
        ConsoleCommand {
            program: pnpm(windows),
            current_dir: workspace.to_owned(),
            args: ["install", "--frozen-lockfile"].map(OsString::from).into(),
        },
        ConsoleCommand {
            program: pnpm(windows),
            current_dir: workspace.to_owned(),
            args: vec![
                "build".into(),
                "--outDir".into(),
                dist.into(),
                "--emptyOutDir".into(),
            ],
        },
    ]
}

pub(crate) fn console_workspace_command(windows: bool, workspace: &Path) -> ConsoleCommand {
    ConsoleCommand {
        program: pnpm(windows),
        current_dir: workspace.to_owned(),
        args: [
            "--filter",
            "awaken-console...",
            "--recursive",
            "list",
            "--depth",
            "-1",
            "--json",
        ]
        .map(OsString::from)
        .into(),
    }
}

/// pnpm owns the dependency closure; exported assets and local TypeScript build
/// configurations own its inputs. Watch source directories and manifests so new
/// source files and export changes invalidate Cargo without watching node_modules
/// or generated distributions. Vendored UI dist remains an input.
pub(crate) fn console_package_inputs(metadata: &[u8]) -> Vec<PathBuf> {
    let packages: serde_json::Value =
        serde_json::from_slice(metadata).expect("parse pnpm workspace graph");
    let packages = packages
        .as_array()
        .expect("pnpm workspace graph must be an array");
    assert!(
        !packages.is_empty(),
        "pnpm workspace graph must not be empty"
    );
    let mut inputs = BTreeSet::new();
    for package in packages {
        let root = Path::new(
            package["path"]
                .as_str()
                .expect("pnpm package requires a path"),
        );
        let manifest = root.join("package.json");
        let body: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read workspace package"))
                .expect("parse workspace package");
        inputs.insert(manifest);
        for key in ["exports", "main", "module", "types"] {
            collect_export_inputs(root, &body[key], &mut inputs);
        }
        // Compiled workspace packages publish dist, but Cargo must observe the
        // compiler's source root and configuration. Vendored distributions have
        // no local build config and remain inputs; local generated outputs do not.
        let build_config = root.join("tsconfig.build.json");
        if build_config.is_file() {
            let config: serde_json::Value =
                serde_json::from_slice(&fs::read(&build_config).unwrap()).unwrap();
            let source = config["compilerOptions"]["rootDir"]
                .as_str()
                .expect("workspace TypeScript build requires rootDir");
            let output = config["compilerOptions"]["outDir"]
                .as_str()
                .expect("workspace TypeScript build requires outDir");
            inputs.remove(&root.join(output));
            collect_export_inputs(root, &serde_json::Value::String(source.into()), &mut inputs);
            if let Some(base) = config["extends"].as_str() {
                collect_export_inputs(root, &serde_json::Value::String(base.into()), &mut inputs);
            }
            inputs.insert(build_config);
        }
    }
    inputs.into_iter().collect()
}

fn collect_export_inputs(root: &Path, value: &serde_json::Value, inputs: &mut BTreeSet<PathBuf>) {
    match value {
        serde_json::Value::String(target) => {
            let relative = Path::new(target.strip_prefix("./").unwrap_or(target));
            assert!(
                relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
                "workspace export must remain inside its package: {target}"
            );
            let first = relative
                .components()
                .next()
                .expect("workspace export requires a path");
            assert!(
                !first.as_os_str().to_string_lossy().contains('*'),
                "workspace export requires a concrete input directory: {target}"
            );
            inputs.insert(root.join(first.as_os_str()));
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_export_inputs(root, value, inputs);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_export_inputs(root, value, inputs);
            }
        }
        serde_json::Value::Null => {}
        _ => panic!("invalid workspace export"),
    }
}

pub(crate) fn write_console_manifest(workspace: &Path, dist: &Path) {
    let web = workspace.join("web");
    let manifest_relative =
        awaken_ui_artifact::CONSOLE_ARTIFACT_MANIFEST_PATH.trim_start_matches('/');
    let manifest_path = dist.join(manifest_relative);
    let _ = fs::remove_file(&manifest_path);
    let mut files = Vec::new();
    collect_files(dist, dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let owned = files
        .iter()
        .filter(|(path, _)| path != manifest_relative)
        .map(|(path, file)| (path.clone(), fs::read(file).expect("read Console asset")))
        .collect::<Vec<_>>();
    let asset_sha256 = awaken_ui_artifact::console_asset_sha256(
        owned
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice())),
    )
    .expect("hash canonical Console assets");
    let manifest = awaken_ui_artifact::ConsoleArtifactManifest {
        contract_version: awaken_ui_artifact::CONSOLE_ARTIFACT_CONTRACT_VERSION,
        product: "agents".into(),
        profiles: vec!["agents".into()],
        asset_sha256,
        api_contract_sha256: file_sha256(&web.join("src/lib/api/types.ts")),
    };
    manifest
        .validate()
        .expect("validate Console artifact manifest");
    fs::create_dir_all(manifest_path.parent().expect("manifest has parent"))
        .expect("create Console manifest directory");
    fs::write(
        manifest_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&manifest).expect("serialize Console manifest")
        ),
    )
    .expect("write Console artifact manifest");
}

fn file_sha256(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read digest input"))
    )
}

pub(crate) fn generate_embedded_assets(dist: &Path, out_dir: &Path) {
    assert!(
        dist.join("index.html").is_file(),
        "frontend build did not produce {}",
        dist.display()
    );
    let mut files = Vec::new();
    collect_files(dist, dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::from(
        "pub(crate) fn embedded_asset(path: &str) -> Option<&'static [u8]> {\n    match path {\n",
    );
    for (relative, absolute) in files {
        generated.push_str(&format!(
            "        {:?} => Some(include_bytes!({:?})),\n",
            relative,
            absolute.to_string_lossy()
        ));
    }
    generated.push_str("        _ => None,\n    }\n}\n");
    fs::write(out_dir.join("embedded_console.rs"), generated)
        .expect("write embedded console asset table");
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("read frontend distribution") {
        let entry = entry.expect("read frontend distribution entry");
        let kind = entry.file_type().expect("read frontend distribution type");
        let path = entry.path();
        assert!(
            kind.is_dir() || kind.is_file(),
            "Console assets must be regular files or directories"
        );
        if kind.is_dir() {
            collect_files(root, &path, files);
        } else {
            files.push((
                path.strip_prefix(root)
                    .expect("asset under dist")
                    .to_string_lossy()
                    .replace('\\', "/"),
                path,
            ));
        }
    }
}

#[cfg(test)]
#[path = "build_support/tests.rs"]
mod tests;
