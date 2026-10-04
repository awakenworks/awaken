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
mod tests {
    use super::*;

    #[test]
    fn console_build_repairs_partial_dependency_state_before_building() {
        // B1 absent/partial/complete dependencies => frozen install before build.
        // B2 Windows/Unix x output path with spaces => one literal output argument,
        // never shared web/dist or shell interpolation. Only the program differs.
        // B3 dependencies publish compiled exports => install AND build run at
        // workspace root, whose build script owns dependency order before Vite.
        // Running only web/build can silently depend on an earlier manual build.
        let workspace = Path::new("workspace with spaces");
        let dist = Path::new("build with spaces/console");
        for (windows, program) in [(false, "pnpm"), (true, "pnpm.cmd")] {
            let [install, build] = console_commands(windows, workspace, dist);
            assert_eq!(install.program, program);
            assert_eq!(install.current_dir, workspace);
            assert_eq!(build.current_dir, workspace);
            assert_eq!(
                install.args,
                ["install", "--frozen-lockfile"].map(OsString::from)
            );
            assert_eq!(build.program, program);
            assert_eq!(
                build.args,
                [
                    "build",
                    "--outDir",
                    "build with spaces/console",
                    "--emptyOutDir"
                ]
                .map(OsString::from)
            );
            let graph = console_workspace_command(windows, workspace);
            assert_eq!(graph.program, program);
            assert_eq!(graph.current_dir, workspace);
        }
    }

    #[test]
    fn console_manifest_binds_exact_assets_and_product_api_without_provenance() {
        // R1 same build -> valid v2 manifest; R2 changed asset or API -> only
        // that digest changes; R3 manifest output is excluded on repeat builds.
        let temp = tempfile::tempdir().unwrap();
        let web = temp.path().join("web");
        let dist = temp.path().join("dist");
        fs::create_dir_all(web.join("src/lib/api")).unwrap();
        fs::create_dir_all(dist.join("assets")).unwrap();
        fs::write(web.join("src/lib/api/types.ts"), "api").unwrap();
        fs::write(dist.join("index.html"), "index").unwrap();
        fs::write(dist.join("assets/app.js"), "app").unwrap();
        write_console_manifest(temp.path(), &dist);
        let first = awaken_ui_artifact::read_console_artifact(&dist).unwrap();
        assert_eq!(first.product, "agents");
        assert_eq!(first.profiles, ["agents"]);
        write_console_manifest(temp.path(), &dist);
        assert_eq!(
            awaken_ui_artifact::read_console_artifact(&dist).unwrap(),
            first
        );
        fs::write(dist.join("assets/app.js"), "changed").unwrap();
        write_console_manifest(temp.path(), &dist);
        let changed_asset = awaken_ui_artifact::read_console_artifact(&dist).unwrap();
        assert_ne!(first.asset_sha256, changed_asset.asset_sha256);
        assert_eq!(first.api_contract_sha256, changed_asset.api_contract_sha256);
        fs::write(web.join("src/lib/api/types.ts"), "changed api").unwrap();
        write_console_manifest(temp.path(), &dist);
        let changed_api = awaken_ui_artifact::read_console_artifact(&dist).unwrap();
        assert_eq!(changed_asset.asset_sha256, changed_api.asset_sha256);
        assert_ne!(
            changed_asset.api_contract_sha256,
            changed_api.api_contract_sha256
        );
    }

    #[test]
    fn console_inputs_follow_workspace_exports_without_a_second_dependency_list() {
        // I1 source export + transitive package; I2 vendored dist + nested/array
        // conditions; I3 duplicate entrypoints => one watch; I4 null exclusion.
        // E1 all published input directories and manifests are watched; E2 the
        // Console's generated dist and dependency installation stay unwatched.
        // I6 compiled exports => source/config changes invalidate Cargo even
        // with unchanged dist; rebuilding dist itself must not invalidate it.
        let temp = tempfile::tempdir().unwrap();
        let mut packages = Vec::new();
        for (name, exports) in [
            ("console", serde_json::Value::Null),
            (
                "projection",
                serde_json::json!({".": {"import":"./src/index.ts", "types":"./src/index.ts"}}),
            ),
            ("compiled", serde_json::json!({".": "./dist/index.js"})),
            (
                "ui",
                serde_json::json!({".":["./dist/index.js"], "./styles":"./dist/style.css", "./private":null}),
            ),
        ] {
            let root = temp.path().join(name);
            fs::create_dir(&root).unwrap();
            if name == "compiled" {
                fs::create_dir(root.join("src")).unwrap();
                fs::write(root.join("tsconfig.json"), "{}").unwrap();
                fs::write(root.join("tsconfig.build.json"),
                    r#"{"extends":"./tsconfig.json","compilerOptions":{"rootDir":"src","outDir":"dist"}}"#).unwrap();
            }
            fs::write(
                root.join("package.json"),
                serde_json::to_vec(&serde_json::json!({"exports":exports})).unwrap(),
            )
            .unwrap();
            packages.push(serde_json::json!({"path":root}));
        }
        let inputs = console_package_inputs(&serde_json::to_vec(&packages).unwrap());
        let expected: BTreeSet<_> = [
            "console/package.json",
            "compiled/package.json",
            "compiled/src",
            "compiled/tsconfig.json",
            "compiled/tsconfig.build.json",
            "projection/package.json",
            "projection/src",
            "ui/package.json",
            "ui/dist",
        ]
        .map(|path| temp.path().join(path))
        .into();
        assert_eq!(inputs.into_iter().collect::<BTreeSet<_>>(), expected);
    }

    #[test]
    fn console_inputs_reject_unknown_graphs_and_escaping_export_roots() {
        // I5 malformed/missing graph coordinates or exports outside the package
        // => fail the build; no silent empty dependency list can enter the cache.
        for metadata in [b"{}".as_slice(), b"[]".as_slice(), b"[{}]".as_slice()] {
            assert!(std::panic::catch_unwind(|| console_package_inputs(metadata)).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        let metadata = serde_json::to_vec(&serde_json::json!([{"path":temp.path()}])).unwrap();
        for target in ["../foreign.js", "/foreign.js", "./*/entry.js"] {
            fs::write(
                temp.path().join("package.json"),
                serde_json::to_vec(&serde_json::json!({"exports":target})).unwrap(),
            )
            .unwrap();
            assert!(
                std::panic::catch_unwind(|| console_package_inputs(&metadata)).is_err(),
                "{target}"
            );
        }
    }

    #[test]
    fn cached_asset_table_survives_an_independent_frontend_build() {
        // A1 two Cargo output owners; A2 later build replaces a different asset
        // generation; A3 development dist is removed. E1 the first generated
        // include_bytes table still compiles and returns its original bytes.
        // Compile and execute the actual generated table to catch dangling paths.
        let temp = tempfile::tempdir().unwrap();
        for (owner, bytes) in [
            ("first", b"first".as_slice()),
            ("second", b"second".as_slice()),
        ] {
            let out = temp.path().join(owner);
            let dist = out.join("console");
            fs::create_dir_all(dist.join("assets")).unwrap();
            fs::write(dist.join("index.html"), "index").unwrap();
            fs::write(dist.join("assets/app.js"), bytes).unwrap();
            generate_embedded_assets(&dist, &out);
        }
        let development = temp.path().join("web/dist");
        fs::create_dir_all(&development).unwrap();
        fs::write(development.join("index.html"), "development").unwrap();
        fs::remove_dir_all(&development).unwrap();
        fs::remove_dir_all(temp.path().join("second/console")).unwrap();
        let source = temp.path().join("probe.rs");
        fs::write(&source, format!("include!({:?}); fn main() {{ assert_eq!(embedded_asset(\"assets/app.js\"), Some(b\"first\".as_slice())); }}", temp.path().join("first/embedded_console.rs"))).unwrap();
        let executable = temp.path().join("probe.exe");
        assert!(
            std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new(executable)
                .status()
                .unwrap()
                .success()
        );
    }
}
