use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConsoleCommand {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<OsString>,
}

fn pnpm(windows: bool) -> &'static str {
    if windows { "pnpm.cmd" } else { "pnpm" }
}

pub(crate) fn console_commands(windows: bool, dist: &Path) -> [ConsoleCommand; 2] {
    [
        ConsoleCommand {
            program: pnpm(windows),
            args: ["install", "--frozen-lockfile"].map(OsString::from).into(),
        },
        ConsoleCommand {
            program: pnpm(windows),
            args: vec![
                "build".into(),
                "--outDir".into(),
                dist.into(),
                "--emptyOutDir".into(),
            ],
        },
    ]
}

pub(crate) fn console_workspace_command(windows: bool) -> ConsoleCommand {
    ConsoleCommand {
        program: pnpm(windows),
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

/// pnpm owns the dependency closure; each package's published entrypoints own
/// its inputs. Watch entrypoint directories as well as manifests so new source
/// files and export changes invalidate Cargo without watching node_modules or
/// the mutable Console development distribution. Vendored UI dist is an input.
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
        let path = entry.expect("read frontend distribution entry").path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
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
        let dist = Path::new("build with spaces/console");
        for (windows, program) in [(false, "pnpm"), (true, "pnpm.cmd")] {
            let [install, build] = console_commands(windows, dist);
            assert_eq!(install.program, program);
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
            assert_eq!(console_workspace_command(windows).program, program);
        }
    }

    #[test]
    fn console_inputs_follow_workspace_exports_without_a_second_dependency_list() {
        // I1 source export + transitive package; I2 vendored dist + nested/array
        // conditions; I3 duplicate entrypoints => one watch; I4 null exclusion.
        // E1 all published input directories and manifests are watched; E2 the
        // Console's generated dist and dependency installation stay unwatched.
        let temp = tempfile::tempdir().unwrap();
        let mut packages = Vec::new();
        for (name, exports) in [
            ("console", serde_json::Value::Null),
            (
                "projection",
                serde_json::json!({".": {"import":"./src/index.ts", "types":"./src/index.ts"}}),
            ),
            (
                "ui",
                serde_json::json!({".":["./dist/index.js"], "./styles":"./dist/style.css", "./private":null}),
            ),
        ] {
            let root = temp.path().join(name);
            fs::create_dir(&root).unwrap();
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
