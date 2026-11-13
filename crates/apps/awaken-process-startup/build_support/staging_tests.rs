use super::*;

fn fixture(workspace: &Path) -> Vec<u8> {
    for (name, contents) in [
        (
            "package.json",
            r#"{"name":"console-build-fixture","private":true,"scripts":{"build":"node src/build.mjs"}}"#,
        ),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        (
            "pnpm-lock.yaml",
            "lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n  packages/compiled: {}\n",
        ),
        (
            "src/build.mjs",
            r#"import assert from 'node:assert/strict';
import fs from 'node:fs';
const [flag, output, empty] = process.argv.slice(2);
assert.equal(flag, '--outDir'); assert.equal(empty, '--emptyOutDir');
assert.equal(fs.existsSync('node_modules/source-sentinel'), false);
assert.equal(fs.existsSync('packages/compiled/dist/stale.js'), false);
fs.mkdirSync('packages/compiled/dist', { recursive: true });
fs.writeFileSync('packages/compiled/dist/fresh.js', 'fresh');
fs.mkdirSync(output, { recursive: true });
fs.writeFileSync(`${output}/index.html`, 'owned console');
"#,
        ),
        ("web/package.json", "{}"),
        ("web/src/lib/api/types.ts", "source api"),
        ("web/index.html", "source index"),
        ("web/tsconfig.json", "{}"),
        ("web/vite.config.ts", "export default {}"),
        (
            "packages/compiled/package.json",
            r#"{"name":"compiled","exports":"./dist/index.js"}"#,
        ),
        (
            "packages/compiled/tsconfig.build.json",
            r#"{"extends":"./tsconfig.json","compilerOptions":{"rootDir":"src","outDir":"dist"}}"#,
        ),
        ("packages/compiled/tsconfig.json", "{}"),
        (
            "packages/compiled/src/index.ts",
            "export const source = true",
        ),
        ("node_modules/source-sentinel", "original install"),
        ("packages/compiled/dist/stale.js", "original build"),
    ] {
        let path = workspace.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    serde_json::to_vec(&serde_json::json!([
        {"path": workspace}, {"path": workspace.join("web")},
        {"path": workspace.join("packages/compiled")}
    ]))
    .unwrap()
}

#[test]
fn staging_owns_installs_outputs_and_repeated_generations_without_source_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("source with spaces");
    let metadata = fixture(&workspace);
    let out = temp.path().join("cargo output with spaces");
    fs::create_dir(&out).unwrap();
    let inputs = console_source_inputs(&workspace, &metadata);
    assert!(
        !inputs
            .iter()
            .any(|path| path.ends_with("dist") || path.ends_with("node_modules"))
    );
    let first = stage_console_workspace(&workspace, &out, &inputs);
    assert_eq!(
        fs::read(first.join("web/src/lib/api/types.ts")).unwrap(),
        b"source api"
    );
    assert!(first.join("packages/compiled/src/index.ts").is_file());
    assert!(!first.join("node_modules").exists());
    assert!(!first.join("packages/compiled/dist").exists());
    fs::create_dir(first.join("node_modules")).unwrap();
    fs::write(first.join("node_modules/retained"), "previous install").unwrap();
    let second = stage_console_workspace(&workspace, &out, &inputs);
    assert_ne!(first, second);
    assert!(first.starts_with(&out) && second.starts_with(&out));
    assert_eq!(
        fs::read(first.join("node_modules/retained")).unwrap(),
        b"previous install"
    );
    assert!(!second.join("node_modules").exists());
    assert_eq!(
        fs::read(workspace.join("node_modules/source-sentinel")).unwrap(),
        b"original install"
    );
    assert_eq!(
        fs::read(workspace.join("packages/compiled/dist/stale.js")).unwrap(),
        b"original build"
    );
    fs::write(workspace.join("web/src/lib/api/types.ts"), "later source").unwrap();
    let dist = first.join("console");
    fs::create_dir(&dist).unwrap();
    fs::write(dist.join("index.html"), "owned console").unwrap();
    write_console_manifest(&first, &dist);
    let artifact = awaken_ui_artifact::read_console_artifact(&dist).unwrap();
    assert_eq!(
        artifact.api_contract_sha256,
        file_sha256(&first.join("web/src/lib/api/types.ts"))
    );
    assert_ne!(
        artifact.api_contract_sha256,
        file_sha256(&workspace.join("web/src/lib/api/types.ts"))
    );
}

#[test]
fn staging_rejects_missing_foreign_and_cache_inputs_without_touching_them() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("source");
    fixture(&workspace);
    let out = temp.path().join("output");
    fs::create_dir(&out).unwrap();
    let foreign = temp.path().join("foreign");
    fs::write(&foreign, "retain").unwrap();
    for input in [
        foreign.clone(),
        workspace.join("../foreign"),
        workspace.join("missing"),
        workspace.join("node_modules/source-sentinel"),
    ] {
        assert!(
            std::panic::catch_unwind(|| stage_console_workspace(
                &workspace,
                &out,
                std::slice::from_ref(&input)
            ))
            .is_err(),
            "{input:?}"
        );
    }
    let foreign_graph = serde_json::to_vec(&serde_json::json!([{"path":temp.path()}])).unwrap();
    assert!(
        std::panic::catch_unwind(|| console_source_inputs(&workspace, &foreign_graph)).is_err()
    );
    assert_eq!(fs::read(foreign).unwrap(), b"retain");
}

#[cfg(unix)]
#[test]
fn staging_rejects_source_symlinks_including_intermediate_package_roots() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("source");
    let metadata = fixture(&workspace);
    let out = temp.path().join("output");
    fs::create_dir(&out).unwrap();
    let foreign = temp.path().join("foreign");
    fs::write(&foreign, "retain").unwrap();
    std::os::unix::fs::symlink(&foreign, workspace.join("web/src/linked")).unwrap();
    let inputs = console_source_inputs(&workspace, &metadata);
    assert!(
        std::panic::catch_unwind(|| stage_console_workspace(&workspace, &out, &inputs)).is_err()
    );
    std::os::unix::fs::symlink(
        workspace.join("packages"),
        workspace.join("linked-packages"),
    )
    .unwrap();
    let graph = serde_json::to_vec(
        &serde_json::json!([{"path":workspace.join("linked-packages/compiled")}]),
    )
    .unwrap();
    assert!(std::panic::catch_unwind(|| console_source_inputs(&workspace, &graph)).is_err());
    assert_eq!(fs::read(foreign).unwrap(), b"retain");
}

#[test]
fn real_noninteractive_frozen_install_and_build_use_only_the_staged_owner() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("source with spaces");
    let metadata = fixture(&workspace);
    let out = temp.path().join("cargo output");
    fs::create_dir(&out).unwrap();
    let inputs = console_source_inputs(&workspace, &metadata);
    let staged = stage_console_workspace(&workspace, &out, &inputs);
    let dist = staged.join("console");
    for spec in console_commands(cfg!(windows), &staged, &dist) {
        let result = spec
            .command()
            .stdin(std::process::Stdio::null())
            .output()
            .expect("start pinned pnpm");
        assert!(
            result.status.success(),
            "{:?}: stdout={}, stderr={}",
            spec.args,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(fs::read(dist.join("index.html")).unwrap(), b"owned console");
    assert_eq!(
        fs::read(staged.join("packages/compiled/dist/fresh.js")).unwrap(),
        b"fresh"
    );
    assert!(!workspace.join("packages/compiled/dist/fresh.js").exists());
    assert_eq!(
        fs::read(workspace.join("node_modules/source-sentinel")).unwrap(),
        b"original install"
    );
    assert_eq!(
        fs::read(workspace.join("packages/compiled/dist/stale.js")).unwrap(),
        b"original build"
    );
}
