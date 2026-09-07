use super::*;

type RenameHook = Box<dyn FnOnce(&Path, &Path) -> std::io::Result<()>>;
thread_local! {
    static RENAME_HOOK: std::cell::RefCell<Option<RenameHook>> = const { std::cell::RefCell::new(None) };
    static OPEN_HOOK: std::cell::RefCell<Option<RenameHook>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn at_open_boundary(name: &std::ffi::OsStr) -> std::io::Result<()> {
    if name == "credential"
        && let Some(hook) = OPEN_HOOK.with(|hook| hook.borrow_mut().take())
    {
        hook(Path::new(name), Path::new(name))?;
    }
    Ok(())
}

#[test]
fn credential_swap_between_handle_opens_cannot_modify_either_file() {
    let dir = TestDir::new("open-race");
    let secret = dir.0.join("credential");
    let original = dir.0.join("original");
    let outside = dir.0.join("outside");
    std::fs::write(&secret, b"secret").unwrap();
    std::fs::write(&outside, b"external sentinel").unwrap();
    let captured_secret = secret.clone();
    let captured_original = original.clone();
    let captured_outside = outside.clone();
    OPEN_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_, _| {
            std::fs::rename(&captured_secret, &captured_original)?;
            std::fs::hard_link(&captured_outside, &captured_secret)
        }))
    });
    let id = directory_identity_nofollow(&dir.0).unwrap();
    assert!(zero_relative_regular_file_nofollow(&dir.0, id, Path::new("credential")).is_err());
    assert_eq!(std::fs::read(original).unwrap(), b"secret");
    assert_eq!(std::fs::read(outside).unwrap(), b"external sentinel");
}

pub(super) fn at_rename_boundary(stage: &Path, destination: &Path) -> std::io::Result<()> {
    if let Some(hook) = RENAME_HOOK.with(|hook| hook.borrow_mut().take()) {
        hook(stage, destination)?;
    }
    Ok(())
}

fn on_rename(hook: impl FnOnce(&Path, &Path) -> std::io::Result<()> + 'static) {
    RENAME_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

struct TestDir(PathBuf);
impl TestDir {
    fn new(name: &str) -> Self {
        let path = test_root(name);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn junction(link: &Path, target: &Path) {
    let result = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "junction creation failed: {:?}",
        result
    );
}

#[test]
fn replacement_failure_preserves_old_marker_for_both_apis() {
    let dir = TestDir::new("replace-failure");
    let marker = dir.0.join("marker");
    std::fs::write(&marker, b"old committed marker").unwrap();
    let lock = try_lock_exclusive(&dir.0.join("lock")).unwrap();
    for locked in [false, true] {
        on_rename(|stage, destination| {
            assert_eq!(std::fs::read(stage)?, b"new committed marker");
            assert_eq!(std::fs::read(destination)?, b"old committed marker");
            Err(std::io::Error::other("injected rename failure"))
        });
        let result = if locked {
            lock.replace_sibling_regular_file_atomic(Path::new("marker"), b"new committed marker")
        } else {
            replace_regular_file_atomic(&marker, b"new committed marker")
        };
        assert!(result.is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"old committed marker");
        assert_eq!(
            lock.sibling_names().unwrap().len(),
            2,
            "failed stage must be reclaimed"
        );
    }
    lock.replace_sibling_regular_file_atomic(Path::new("marker"), b"complete new marker")
        .unwrap();
    assert_eq!(std::fs::read(marker).unwrap(), b"complete new marker");
}

#[test]
fn os_replace_failure_keeps_destination() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let dir = TestDir::new("os-replace-failure");
    let marker = dir.0.join("marker");
    std::fs::write(&marker, b"old").unwrap();
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&marker)
        .unwrap();
    assert!(replace_regular_file_atomic(&marker, b"new").is_err());
    assert_eq!(std::fs::read(&marker).unwrap(), b"old");
}

#[test]
fn replacement_crash_child() {
    let Some(path) = std::env::var_os("AWAKEN_FS_CRASH_TEST_MARKER") else {
        return;
    };
    on_rename(|_, destination| {
        assert_eq!(std::fs::read(destination)?, b"old");
        // exit bypasses destructors, modelling loss of the process at the boundary.
        std::process::exit(73);
    });
    replace_regular_file_atomic(Path::new(&path), b"new").unwrap();
    panic!("crash boundary was not reached");
}

#[test]
fn process_exit_at_replace_boundary_keeps_recoverable_marker() {
    let dir = TestDir::new("crash-replace");
    let marker = dir.0.join("marker");
    std::fs::write(&marker, b"old").unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows_tests::replacement_crash_child",
            "--nocapture",
        ])
        .env("AWAKEN_FS_CRASH_TEST_MARKER", &marker)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    assert_eq!(std::fs::read(&marker).unwrap(), b"old");
    let stage = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p != &marker)
        .expect("flushed private stage survives crash");
    assert_eq!(std::fs::read(stage).unwrap(), b"new");
    replace_regular_file_atomic(&marker, b"recovered").unwrap();
    assert_eq!(std::fs::read(marker).unwrap(), b"recovered");
}

#[test]
fn intermediate_junction_cannot_escape_for_reads_writes_or_cleanup() {
    let dir = TestDir::new("junction");
    let root = dir.0.join("root");
    let outside = dir.0.join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim"), b"external sentinel").unwrap();
    junction(&root.join("jump"), &outside);
    let id = directory_identity_nofollow(&root).unwrap();
    assert!(read_regular_tree_nofollow(&root, id, Path::new("jump")).is_err());
    assert!(read_regular_tree_nofollow(&root, id, Path::new("jump/missing")).is_err());
    assert!(remove_relative_entry_exact(&root, id, Path::new("jump/victim")).is_err());
    assert!(zero_relative_regular_file_nofollow(&root, id, Path::new("jump/victim")).is_err());
    assert!(
        write_relative_file_atomic(&root, id, Path::new("jump/victim"), b"bad", 0o600).is_err()
    );
    assert!(create_relative_directory_all(&root, id, Path::new("jump/new")).is_err());
    assert!(set_relative_directory_mode(&root, id, Path::new("jump"), 0o700).is_err());
    assert_eq!(
        std::fs::read(outside.join("victim")).unwrap(),
        b"external sentinel"
    );
    // Exact cleanup may unlink the junction itself, without traversing it.
    remove_relative_entry_exact(&root, id, Path::new("jump")).unwrap();
    assert!(!root.join("jump").exists());
    assert_eq!(
        std::fs::read(outside.join("victim")).unwrap(),
        b"external sentinel"
    );
}

#[test]
fn shred_rejects_hard_links_and_preserves_external_bytes() {
    let dir = TestDir::new("hardlink");
    let root = dir.0.join("root");
    std::fs::create_dir(&root).unwrap();
    let outside = dir.0.join("outside");
    std::fs::write(&outside, b"external sentinel").unwrap();
    std::fs::hard_link(&outside, root.join("secret")).unwrap();
    let id = directory_identity_nofollow(&root).unwrap();
    assert!(zero_relative_regular_file_nofollow(&root, id, Path::new("secret")).is_err());
    assert!(read_regular_tree_nofollow(&root, id, Path::new("")).is_err());
    assert!(replace_regular_file_atomic(&root.join("secret"), b"new").is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"external sentinel");
}

#[test]
fn missing_secrets_are_idempotent_but_foreign_or_missing_roots_are_not() {
    let dir = TestDir::new("missing-secret");
    let root = dir.0.join("root");
    std::fs::create_dir(&root).unwrap();
    let id = directory_identity_nofollow(&root).unwrap();
    for name in ["secret", "gone/secret"] {
        zero_relative_regular_file_nofollow(&root, id, Path::new(name)).unwrap();
        zero_relative_regular_file_nofollow(&root, id, Path::new(name)).unwrap();
    }
    std::fs::write(root.join("secret"), b"credential").unwrap();
    zero_relative_regular_file_nofollow(&root, id, Path::new("secret")).unwrap();
    assert_eq!(std::fs::read(root.join("secret")).unwrap(), [0; 10]);
    let other = dir.0.join("other");
    std::fs::create_dir(&other).unwrap();
    assert!(zero_relative_regular_file_nofollow(&other, id, Path::new("secret")).is_err());
    remove_directory_tree_exact(&root, id).unwrap();
    assert!(zero_relative_regular_file_nofollow(&root, id, Path::new("secret")).is_err());
}

#[test]
fn competing_directory_publication_preserves_destination_and_stage() {
    let dir = TestDir::new("publish-race");
    let lock = try_lock_exclusive(&dir.0.join("lock")).unwrap();
    for locked in [false, true] {
        let suffix = if locked { "locked" } else { "free" };
        let stage = dir.0.join(format!("stage-{suffix}"));
        let target = dir.0.join(format!("target-{suffix}"));
        std::fs::create_dir(&stage).unwrap();
        let stage_id = directory_identity_nofollow(&stage).unwrap();
        let identity = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured = identity.clone();
        on_rename(move |_, destination| {
            // Deterministic competing process at the validation/rename boundary.
            let path = destination.to_owned();
            std::thread::spawn(move || std::fs::create_dir(path))
                .join()
                .unwrap()?;
            captured.set(Some(directory_identity_nofollow(destination)?));
            Ok(())
        });
        let result = if locked {
            lock.publish_sibling_directory_noreplace(
                Path::new(stage.file_name().unwrap()),
                Path::new(target.file_name().unwrap()),
            )
        } else {
            publish_directory_noreplace(&stage, &target)
        };
        assert!(result.is_err());
        assert_eq!(
            directory_identity_nofollow(&target).unwrap(),
            identity.get().unwrap()
        );
        assert_eq!(directory_identity_nofollow(&stage).unwrap(), stage_id);
    }
}

#[test]
fn retained_parent_prevents_directory_swap() {
    let dir = TestDir::new("parent-swap");
    let root = dir.0.join("root");
    std::fs::create_dir_all(root.join("parent")).unwrap();
    let guard = windows::PinnedDir::open(&root.join("parent")).unwrap();
    assert!(std::fs::rename(root.join("parent"), root.join("moved")).is_err());
    assert!(std::fs::rename(&root, dir.0.join("moved-root")).is_err());
    drop(guard);
    std::fs::rename(root.join("parent"), root.join("moved")).unwrap();
}

#[test]
fn windows_alias_paths_are_rejected() {
    let dir = TestDir::new("aliases");
    let id = directory_identity_nofollow(&dir.0).unwrap();
    for path in [
        "../outside",
        "secret:stream",
        "NUL",
        "name.",
        "name ",
        "bad\0ignored",
    ] {
        assert!(
            write_relative_file_atomic(&dir.0, id, Path::new(path), b"bad", 0o600).is_err(),
            "{path:?}"
        );
    }
}

fn test_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "awaken-sandbox-fs-{name}-{}-{}",
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn windows_local_sandbox_round_trip() {
    let parent = test_root("round-trip");
    std::fs::create_dir_all(&parent).unwrap();
    let destination = parent.join("sandbox");
    let (stage, stage_identity) = create_private_directory_stage(&destination).unwrap();
    publish_directory_noreplace(&stage, &destination).unwrap();
    assert_eq!(
        directory_identity_nofollow(&destination).unwrap(),
        stage_identity
    );

    create_relative_directory_all(
        &destination,
        stage_identity,
        Path::new("mnt/session/outputs"),
    )
    .unwrap();
    write_relative_file_atomic(
        &destination,
        stage_identity,
        Path::new("mnt/session/outputs/result.txt"),
        b"MODEL READY",
        0o600,
    )
    .unwrap();
    let files = read_regular_tree_nofollow(
        &destination,
        stage_identity,
        Path::new("mnt/session/outputs"),
    )
    .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].bytes, b"MODEL READY");

    remove_relative_entry_exact(
        &destination,
        stage_identity,
        Path::new("mnt/session/outputs/result.txt"),
    )
    .unwrap();
    remove_directory_tree_exact(&destination, stage_identity).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn windows_exclusive_lock_blocks_second_owner() {
    let parent = test_root("lock");
    std::fs::create_dir_all(&parent).unwrap();
    let path = parent.join("realization.lock");
    let first = try_lock_exclusive(&path).unwrap();
    let error = try_lock_exclusive(&path).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    drop(first);
    try_lock_exclusive(&path).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}
