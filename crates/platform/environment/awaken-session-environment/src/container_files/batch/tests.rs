use super::*;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

// Run the actual portable remote program against a private temporary workspace.
// Only transport coordinates are remapped; quoting, validation, permissions and
// publication execute unchanged. This fixture launches POSIX sh/chmod/mv on
// Unix; Windows retains the pure invalid-input row below, while real Linux
// container execution remains in live E2E.
#[cfg(unix)]
fn execute(
    command: &pc::Command,
    workspace: &std::path::Path,
    tool_path: Option<&std::path::Path>,
) -> std::process::ExitStatus {
    let mut argv = command.argv.clone();
    for value in argv.iter_mut().skip(4).take(4) {
        if let Some(relative) = value.strip_prefix("/workspace") {
            *value = format!("{}{relative}", workspace.display());
        }
    }
    let mut process = std::process::Command::new(&argv[0]);
    process.args(&argv[1..]).current_dir(workspace);
    if let Some(tools) = tool_path {
        process.env("PATH", format!("{}:/usr/bin:/bin", tools.display()));
    }
    process.status().unwrap()
}

/// A private workspace whose published read-only trees (B3) are removed with
/// it: `TempDir` cannot unlink entries of a directory without write bits, so
/// owner access is restored first, never through a symlink.
#[cfg(unix)]
struct Workspace(tempfile::TempDir);

#[cfg(unix)]
impl Workspace {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[cfg(unix)]
impl Drop for Workspace {
    fn drop(&mut self) {
        fn restore_owner_access(path: &std::path::Path) {
            let Ok(metadata) = fs::symlink_metadata(path) else {
                return;
            };
            if metadata.is_dir() {
                let mode = metadata.permissions().mode() | 0o700;
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
                for entry in fs::read_dir(path).into_iter().flatten().flatten() {
                    restore_owner_access(&entry.path());
                }
            }
        }
        restore_owner_access(self.0.path());
    }
}

#[cfg(unix)]
#[test]
fn small_trees_use_one_transfer_and_one_fenced_publication() {
    // B1 empty/one/many small files => one bounded transfer + final publication.
    // The separate publication spawn preserves the current authority fence.
    // B2 binary, empty,
    // nested, Unicode and shell-special paths => exact bytes, not shell code.
    // B3 executable/non-executable => preserved execute bit and no write bits.
    for count in [0, 1, 50] {
        let workspace = Workspace::new();
        let mut files = (0..count)
            .map(|i| (format!("nested/{i}.bin"), vec![0, 255, i as u8], false))
            .collect::<Vec<_>>();
        if count > 0 {
            files.extend([
                (
                    "空 格/quote'$(touch injected).bin".into(),
                    Vec::new(),
                    false,
                ),
                (
                    "scripts/run.sh".into(),
                    b"#!/bin/sh\nprintf RUN_OK".to_vec(),
                    true,
                ),
            ]);
        }
        let (_, batches) = commands(".skills/example", &files, 1).unwrap();
        assert_eq!(batches.len(), 2, "B1 count={count}");
        assert!(execute(&batches[0], workspace.path(), None).success());
        assert!(
            !workspace.path().join(".skills/example").exists(),
            "B1 transfer cannot publish"
        );
        assert!(execute(&batches[1], workspace.path(), None).success());
        let root = workspace.path().join(".skills/example");
        for (relative, bytes, executable) in &files {
            let path = root.join(relative);
            assert_eq!(fs::read(&path).unwrap(), *bytes, "B2");
            #[cfg(unix)]
            {
                let mode = fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o222, 0, "B3");
                assert_eq!(mode & 0o100 != 0, *executable, "B3");
            }
        }
        assert!(
            !workspace.path().join("injected").exists(),
            "B2 no evaluation"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o222,
            0,
            "B3 directory"
        );
    }
}

#[cfg(unix)]
#[test]
fn large_tree_batches_preserve_old_tree_until_final_publication() {
    // B4 bytes exceed argv bound => bounded chunks, first setup once, final
    // publication once. Every intermediate success preserves the old tree.
    let workspace = Workspace::new();
    let root = workspace.path().join("bundle");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("old"), b"old").unwrap();
    let contents = (0..150_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let (_, batches) =
        commands("bundle", &[("binary".into(), contents.clone(), false)], 2).unwrap();
    assert!(batches.len() > 1);
    for (i, command) in batches.iter().enumerate() {
        assert!(
            command.argv.iter().map(|a| a.len() + 1).sum::<usize>() <= COMMAND_BYTES,
            "B4 bounded argv"
        );
        assert!(execute(command, workspace.path(), None).success());
        if i + 1 < batches.len() {
            assert_eq!(
                fs::read(root.join("old")).unwrap(),
                b"old",
                "B4 no partial publication"
            );
        }
    }
    assert_eq!(fs::read(root.join("binary")).unwrap(), contents);
    assert!(!root.join("old").exists());
}

#[test]
fn every_invalid_entry_is_rejected_before_any_command_is_returned() {
    // B5 invalid first/last path, normalized duplicate, file/directory conflict,
    // or an oversized path => reject whole input before remote setup/mutation.
    assert!(commands("", &[], 3).is_err(), "B5 cannot replace workspace");
    for invalid in ["../escape", "/absolute", "a\\b", "", "nul\0name"] {
        for last in [false, true] {
            let mut files = vec![("valid".into(), b"valid".to_vec(), false)];
            files.insert(usize::from(last), (invalid.into(), Vec::new(), false));
            assert!(commands("bundle", &files, 3).is_err(), "B5 {invalid:?}");
        }
    }
    for paths in [["a//b", "a/b"], ["a", "a/b"], ["a/b", "a"]] {
        assert!(
            commands("bundle", &paths.map(|p| (p.into(), Vec::new(), false)), 3).is_err(),
            "B5 {paths:?}"
        );
    }
    assert!(
        commands(
            "bundle",
            &[("x".repeat(COMMAND_BYTES), Vec::new(), false)],
            3
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn failed_append_or_publication_preserves_old_tree_and_allows_cleanup() {
    // B6 decoding fails after a previous batch, or final rename fails after
    // moving the old root => reject, restore/retain old root, remove only stage.
    for publication_failure in [false, true] {
        let workspace = Workspace::new();
        let root = workspace.path().join("bundle");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("old"), b"old").unwrap();
        let files = [("binary".into(), vec![42; 80_000], false)];
        let (stage, mut batches) = commands("bundle", &files, 4).unwrap();
        let tools = workspace.path().join("tools");
        fs::create_dir(&tools).unwrap();
        if publication_failure {
            fs::write(tools.join("mv"), "#!/bin/sh\ncase \"$2\" in *.awaken-tree-stage-*) exit 67;; esac\nexec /bin/mv \"$@\"\n").unwrap();
            #[cfg(unix)]
            fs::set_permissions(tools.join("mv"), fs::Permissions::from_mode(0o700)).unwrap();
        } else {
            batches[1].argv[13] = "!".into();
        }
        let mut failed = false;
        for command in &batches {
            if !execute(command, workspace.path(), Some(&tools)).success() {
                failed = true;
                break;
            }
        }
        assert!(failed, "B6 expected injected failure");
        assert_eq!(
            fs::read(root.join("old")).unwrap(),
            b"old",
            "B6 old tree intact"
        );
        let cleanup = cleanup_command(&stage);
        assert!(execute(&cleanup, workspace.path(), None).success());
        assert!(
            !workspace
                .path()
                .join(stage.strip_prefix("/workspace/").unwrap())
                .exists(),
            "B6 cleanup"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_roots_and_stages_are_never_followed() {
    // B7 symlink ancestor or pre-existing stage => reject without touching its
    // target; cleanup likewise cannot chmod/remove through a stage symlink.
    for ancestor in [false, true] {
        let workspace = Workspace::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"unchanged").unwrap();
        let (stage, batches) = commands(
            "bundle/tree",
            &[("file".into(), b"bytes".to_vec(), false)],
            5,
        )
        .unwrap();
        if ancestor {
            symlink(outside.path(), workspace.path().join("bundle")).unwrap();
        } else {
            fs::create_dir(workspace.path().join("bundle")).unwrap();
            symlink(
                outside.path(),
                workspace
                    .path()
                    .join(stage.strip_prefix("/workspace/").unwrap()),
            )
            .unwrap();
        }
        assert!(
            !execute(&batches[0], workspace.path(), None).success(),
            "B7"
        );
        assert!(
            !execute(&cleanup_command(&stage), workspace.path(), None).success(),
            "B7 cleanup"
        );
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            b"unchanged"
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    }
}
