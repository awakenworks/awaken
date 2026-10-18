//! The installation's platform Workspace marker.
//!
//! A durable installation names its platform-managed local Workspace once, in
//! `<root>/platform-workspace-id`, and every later process startup over the
//! same root must read back exactly that coordinate. Publication is atomic and
//! no-replace: concurrent first startups converge on one winner, an exact
//! replay succeeds, and a different or malformed marker fails closed without
//! being replaced. The policy-free no-replace syscall stays owned by
//! `awaken-sandbox-fs`; minting a fresh coordinate stays with the Runtime Host
//! (`ExecutorComponents::mint_local_workspace`), which owns its format.
//!
//! Consumers: the CLI's process stores and installation binding, the Worker's
//! host and container realization namespace, and the scenario host. All pass
//! the resolved coordinate to the Runtime Host as a value.

use std::io;
use std::path::Path;

const MARKER_FILE: &str = "platform-workspace-id";

/// Read the exact existing installation coordinate without creating a
/// directory or publishing identity.
pub fn read_at(root: &Path) -> io::Result<Option<String>> {
    read_marker(&root.join(MARKER_FILE))
}

/// Atomically publish one caller-selected coordinate through the marker.
/// Exact replay succeeds; a concurrent or pre-existing different coordinate
/// fails closed without replacement.
pub fn publish_at(root: &Path, workspace_id: &str) -> io::Result<String> {
    publish_marker(&root.join(MARKER_FILE), workspace_id)
}

/// Resolve the installation coordinate under `root`, publishing `mint()` when
/// none exists yet. A concurrent first startup that wins the publication race
/// decides the coordinate for both.
///
/// # Panics
///
/// When the marker cannot be read, is malformed or not a regular file, or
/// cannot be persisted: startup must never proceed under a guessed Workspace.
pub fn provision_at(root: &Path, mint: impl FnOnce() -> String) -> String {
    let path = root.join(MARKER_FILE);
    match read_marker(&path) {
        Ok(Some(existing)) => return existing,
        Ok(None) => {}
        Err(error) => panic!(
            "read platform workspace identity `{}`: {error}",
            path.display()
        ),
    }
    match publish_marker(&path, &mint()) {
        Ok(workspace) => workspace,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_marker(&path)
            .expect("read concurrently published platform workspace identity")
            .expect("concurrent platform workspace publisher wrote an identity"),
        Err(error) => panic!(
            "persist platform workspace identity `{}`: {error}",
            path.display()
        ),
    }
}

fn publish_marker(path: &Path, workspace_id: &str) -> io::Result<String> {
    if workspace_id.is_empty() || workspace_id.trim() != workspace_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "platform Workspace identity must be non-empty with no surrounding whitespace",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match awaken_sandbox_fs::publish_file_noreplace(path, workspace_id.as_bytes()) {
        Ok(()) => Ok(workspace_id.to_owned()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = read_marker(path)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "concurrent platform Workspace publisher left no identity",
                )
            })?;
            if existing == workspace_id {
                Ok(existing)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "platform Workspace identity mismatch: requested {workspace_id:?}, found {existing:?}"
                    ),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn read_marker(path: &Path) -> io::Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    let aliased = {
        use std::os::unix::fs::MetadataExt as _;
        metadata.nlink() != 1
    };
    #[cfg(not(unix))]
    let aliased = false;
    if !metadata.is_file() || metadata.file_type().is_symlink() || aliased {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("`{}` is not a regular identity file", path.display()),
        ));
    }
    let existing = std::fs::read_to_string(path)?;
    let existing = existing.trim();
    if existing.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("`{}` contains an empty identity", path.display()),
        ));
    }
    Ok(Some(existing.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_workspace_identity_publication_is_atomic_and_fail_closed() {
        // Cause/effect table: C1 identity absent/valid/empty/non-regular; C2 one
        // or two startup processes publish concurrently; C3 a deployment asks
        // to publish an exact coordinate. R1 absent+one writes a
        // complete durable identity; R2 absent+two returns the same winning
        // identity to both and never truncates it; R3 valid replays byte-for-byte;
        // R4 empty or non-regular fails closed without replacement. The
        // policy-free no-replace syscall remains owned by awaken-sandbox-fs.
        // The read-only owner projects the same table: absent=>None, valid=>the
        // exact coordinate, malformed/non-regular=>error without publication. R5 an
        // exact first publish/replay returns that coordinate, while a conflicting
        // C3 fails without replacing it.
        let directory = tempfile::tempdir().unwrap();
        assert!(
            read_at(directory.path()).unwrap().is_none(),
            "R1 read-only absent probe"
        );
        let root = std::sync::Arc::new(directory.path().to_path_buf());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |candidate: &'static str,
                     root: std::sync::Arc<std::path::PathBuf>,
                     barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                barrier.wait();
                provision_at(root.as_path(), || candidate.to_owned())
            })
        };
        let one = spawn("workspace-candidate-one", root.clone(), barrier.clone());
        let two = spawn("workspace-candidate-two", root.clone(), barrier);
        let one = one.join().unwrap();
        let two = two.join().unwrap();
        assert_eq!(one, two, "R2");
        assert!(
            one == "workspace-candidate-one" || one == "workspace-candidate-two",
            "R1 a minted candidate wins"
        );
        assert_eq!(
            provision_at(root.as_path(), || "workspace-late".to_owned()),
            one,
            "R3"
        );
        assert_eq!(
            read_at(root.as_path()).unwrap().as_deref(),
            Some(one.as_str()),
            "R3 read-only exact identity"
        );

        let exact = tempfile::tempdir().unwrap();
        assert_eq!(
            publish_at(exact.path(), "workspace-exact").unwrap(),
            "workspace-exact",
            "R5 first publish"
        );
        assert_eq!(
            publish_at(exact.path(), "workspace-exact").unwrap(),
            "workspace-exact",
            "R5 exact replay"
        );
        assert_eq!(
            publish_at(exact.path(), "workspace-other")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists,
            "R5 mismatch"
        );
        assert_eq!(
            std::fs::read_to_string(exact.path().join("platform-workspace-id")).unwrap(),
            "workspace-exact",
            "R5 no replacement"
        );

        let invalid = tempfile::tempdir().unwrap();
        std::fs::write(invalid.path().join("platform-workspace-id"), b"").unwrap();
        assert!(
            read_at(invalid.path()).is_err(),
            "R4 read-only malformed probe"
        );
        assert!(
            std::panic::catch_unwind(|| provision_at(invalid.path(), || "workspace-new".into()))
                .is_err(),
            "R4"
        );

        #[cfg(unix)]
        {
            let linked = tempfile::tempdir().unwrap();
            let target = linked.path().join("foreign");
            std::fs::write(&target, b"foreign-workspace").unwrap();
            std::os::unix::fs::symlink(&target, linked.path().join("platform-workspace-id"))
                .unwrap();
            assert!(
                std::panic::catch_unwind(|| provision_at(linked.path(), || "workspace-new".into()))
                    .is_err(),
                "R4"
            );
            assert_eq!(std::fs::read(target).unwrap(), b"foreign-workspace", "R4");

            let hardlinked = tempfile::tempdir().unwrap();
            let target = hardlinked.path().join("foreign");
            std::fs::write(&target, b"foreign-workspace").unwrap();
            std::fs::hard_link(&target, hardlinked.path().join("platform-workspace-id")).unwrap();
            assert!(
                std::panic::catch_unwind(|| {
                    provision_at(hardlinked.path(), || "workspace-new".into())
                })
                .is_err(),
                "R4"
            );
            assert_eq!(std::fs::read(target).unwrap(), b"foreign-workspace", "R4");
        }
    }
}
