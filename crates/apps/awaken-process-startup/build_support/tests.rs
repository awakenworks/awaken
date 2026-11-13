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
        assert_eq!(
            graph.args,
            ["--recursive", "list", "--depth", "-1", "--json"].map(OsString::from)
        );
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

fn probe_compiler(
    rustc: &std::ffi::OsStr,
    wrapper: Option<&std::ffi::OsStr>,
) -> std::process::Command {
    match wrapper.filter(|program| !program.is_empty()) {
        Some(wrapper) => {
            let mut command = std::process::Command::new(wrapper);
            command.arg(rustc);
            command
        }
        None => std::process::Command::new(rustc),
    }
}

#[test]
fn asset_probe_respects_the_cargo_compiler_wrapper_protocol() {
    // W1 absent/empty wrapper => invoke rustc directly; W2 a configured wrapper
    // receives the real compiler as its first argument. Paths containing spaces
    // remain individual OS arguments; the probe never invokes a shell.
    let rustc = std::ffi::OsStr::new("compiler with spaces/rustc");
    let wrapper = std::ffi::OsStr::new("wrapper with spaces/cache");
    for (configured, expected_program, prefix) in [
        (None, rustc, Vec::new()),
        (Some(std::ffi::OsStr::new("")), rustc, Vec::new()),
        (Some(wrapper), wrapper, vec![rustc]),
    ] {
        let mut command = probe_compiler(rustc, configured);
        let arguments = ["--edition=2024", "source with spaces.rs", "-o", "probe.exe"];
        command.args(arguments);
        assert_eq!(command.get_program(), expected_program);
        let expected = prefix
            .into_iter()
            .chain(arguments.into_iter().map(std::ffi::OsStr::new))
            .collect::<Vec<_>>();
        assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
    }
}

#[test]
fn cached_asset_table_survives_an_independent_frontend_build() {
    // A1 two Cargo output owners; A2 later build replaces a different asset
    // generation; A3 development dist is removed. E1 the first generated
    // include_bytes table still compiles and returns its original bytes.
    // Compile and execute the actual generated table to catch dangling paths.
    // The probe is a Cargo output too: use the same executable filesystem as
    // this test binary rather than assuming the ambient temporary volume can
    // execute freshly built programs. Each generation still has its own owner.
    let test_executable = std::env::current_exe().expect("locate the Cargo test output");
    let temp = tempfile::Builder::new()
        .prefix("embedded-assets-probe-")
        .tempdir_in(test_executable.parent().expect("test output directory"))
        .expect("create an independent executable Cargo output owner");
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
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let wrapper = std::env::var_os("RUSTC_WRAPPER");
    assert!(
        probe_compiler(&rustc, wrapper.as_deref())
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let result = std::process::Command::new(&executable)
        .output()
        .unwrap_or_else(|error| panic!("execute generated asset probe {executable:?}: {error}"));
    assert!(
        result.status.success(),
        "generated table must retain its first owner's bytes: status={:?}, stdout={}, stderr={}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
}

#[path = "staging_tests.rs"]
mod staging_tests;
