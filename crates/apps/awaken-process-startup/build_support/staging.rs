use super::console_package_inputs;
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Preserve pnpm's full importer graph and the existing source-input owner.
/// Defaults used by Vite/TypeScript are inputs even without package exports.
pub(crate) fn console_source_inputs(workspace: &Path, metadata: &[u8]) -> Vec<PathBuf> {
    let packages: serde_json::Value =
        serde_json::from_slice(metadata).expect("parse pnpm workspace graph");
    for package in packages
        .as_array()
        .expect("pnpm workspace graph must be an array")
    {
        let root = Path::new(
            package["path"]
                .as_str()
                .expect("pnpm package requires a path"),
        );
        let relative = checked_relative(workspace, root);
        let mut component_path = workspace.to_owned();
        for component in relative.components() {
            component_path.push(component);
            assert!(
                !fs::symlink_metadata(&component_path)
                    .expect("inspect workspace package path")
                    .file_type()
                    .is_symlink(),
                "workspace package must not follow a source symlink"
            );
        }
    }
    let mut inputs: BTreeSet<_> = console_package_inputs(metadata).into_iter().collect();
    for package in packages.as_array().unwrap() {
        let root = Path::new(package["path"].as_str().unwrap());
        for name in ["src", "index.html", "tsconfig.json", "vite.config.ts"] {
            let input = root.join(name);
            if input.try_exists().expect("inspect Console input") {
                inputs.insert(input);
            }
        }
    }
    for name in ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"] {
        inputs.insert(workspace.join(name));
    }
    inputs.into_iter().collect()
}

/// All mutable frontend state belongs to this Cargo invocation. Do not reuse a
/// previous install: pnpm may need an interactive purge when its layout changes.
/// Keep completed generations alive for the generated include_bytes! table.
pub(crate) fn stage_console_workspace(
    workspace: &Path,
    out_dir: &Path,
    inputs: &[PathBuf],
) -> PathBuf {
    let mut generation = 0u64;
    let staged = loop {
        let path = out_dir.join(format!("ui-{}-{generation}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => generation += 1,
            Err(error) => panic!("create Cargo-owned Console workspace: {error}"),
        }
    };
    for input in inputs {
        let relative = checked_relative(workspace, input);
        copy_input(input, &staged.join(relative));
    }
    staged
}

fn checked_relative<'a>(workspace: &Path, input: &'a Path) -> &'a Path {
    let relative = input
        .strip_prefix(workspace)
        .expect("Console input must remain inside its workspace");
    assert!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Console input must not traverse outside its workspace"
    );
    assert!(
        !relative.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("node_modules" | ".git" | "target" | ".cache" | ".pnpm-store")
            )
        }),
        "Console input must not import installation or build caches"
    );
    relative
}

fn copy_input(source: &Path, destination: &Path) {
    let kind = fs::symlink_metadata(source)
        .expect("read Console source input")
        .file_type();
    assert!(
        kind.is_file() || kind.is_dir(),
        "Console inputs must be regular files or directories"
    );
    if kind.is_dir() {
        fs::create_dir_all(destination).expect("create staged Console source directory");
        for entry in fs::read_dir(source).expect("read Console source directory") {
            let entry = entry.expect("read Console source entry");
            assert!(
                !matches!(
                    entry.file_name().to_str(),
                    Some("node_modules" | ".git" | "target" | ".cache" | ".pnpm-store")
                ),
                "Console source directory must not contain installation or build caches"
            );
            copy_input(&entry.path(), &destination.join(entry.file_name()));
        }
    } else {
        fs::create_dir_all(destination.parent().expect("staged input has parent"))
            .expect("create staged Console input parent");
        fs::copy(source, destination).expect("copy Console source input");
    }
}
