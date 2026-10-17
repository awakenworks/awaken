// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Test-only Windows publication, replacement and original NTFS identity observations.
// @formal reason: These executable regressions classify effects, not a formal-proof claim.
#![cfg(windows)]
use awaken_sandbox_fs::*;
use std::path::{Path, PathBuf};

#[test]
fn windows_long_sibling_publication_keeps_exact_identity_and_noreplace() {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    // Cause/effect matrix: short/over-MAX_PATH NTFS parents, ordinary/verbatim/
    // forward-slash spellings, absent/occupied file and directory destinations.
    // Publication and replacement must retain the original locked parent,
    // preserve occupied bytes, and move exactly the staged directory identity.
    for long in [false, true] {
        for spelling in ["ordinary", "verbatim", "forward-slash"] {
            let temp = tempfile::tempdir().unwrap();
            let mut parent = temp.path().join("publication-目录");
            std::fs::create_dir(&parent).unwrap();
            if long {
                while parent.as_os_str().encode_wide().count() < 320 {
                    parent.push("nested-filesystem-path");
                    std::fs::create_dir(&parent).unwrap();
                }
            }
            if let Some(std::path::Component::Prefix(prefix)) = parent.components().next()
                && matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
            {
                let wide = parent.as_os_str().encode_wide().collect::<Vec<_>>();
                parent = PathBuf::from(std::ffi::OsString::from_wide(&wide[4..]));
            }
            let parent = match spelling {
                "verbatim" => {
                    let mut name = std::ffi::OsString::from(r"\\?\");
                    name.push(parent.as_os_str());
                    PathBuf::from(name)
                }
                "forward-slash" => PathBuf::from(parent.to_str().unwrap().replace('\\', "/")),
                _ => parent,
            };
            let expected_parent = directory_identity_nofollow(&parent).unwrap();
            let lock = try_lock_exclusive(&parent.join(".lock")).unwrap();
            let marker = Path::new("marker-🧪.json");
            lock.publish_sibling_file_noreplace(marker, b"original")
                .unwrap_or_else(|error| panic!("long={long} spelling={spelling}: {error:?}"));
            assert_eq!(
                lock.read_sibling_regular_file_nofollow(marker).unwrap(),
                b"original"
            );
            assert!(
                lock.publish_sibling_file_noreplace(marker, b"foreign")
                    .is_err()
            );
            assert_eq!(std::fs::read(parent.join(marker)).unwrap(), b"original");
            lock.replace_sibling_regular_file_atomic(marker, b"replacement")
                .unwrap();
            assert_eq!(
                lock.read_sibling_regular_file_nofollow(marker).unwrap(),
                b"replacement"
            );

            let stage = Path::new("private-stage");
            let published = Path::new("published");
            let staged_identity = lock.create_sibling_directory_noreplace(stage).unwrap();
            std::fs::write(parent.join(stage).join("PAYLOAD"), b"stage").unwrap();
            lock.publish_sibling_directory_noreplace(stage, published)
                .unwrap();
            assert_eq!(
                directory_identity_nofollow(&parent.join(published)).unwrap(),
                staged_identity
            );
            assert!(!parent.join(stage).exists());
            assert_eq!(
                std::fs::read(parent.join(published).join("PAYLOAD")).unwrap(),
                b"stage"
            );
            let retained_stage = lock.create_sibling_directory_noreplace(stage).unwrap();
            assert!(
                lock.publish_sibling_directory_noreplace(stage, published)
                    .is_err()
            );
            assert_eq!(
                directory_identity_nofollow(&parent.join(stage)).unwrap(),
                retained_stage
            );
            assert_eq!(
                directory_identity_nofollow(&parent.join(published)).unwrap(),
                staged_identity
            );
            assert_eq!(
                std::fs::read(parent.join(published).join("PAYLOAD")).unwrap(),
                b"stage"
            );
            assert_eq!(
                directory_identity_nofollow(&parent).unwrap(),
                expected_parent
            );
            assert!(
                lock.sibling_names()
                    .unwrap()
                    .iter()
                    .all(|name| !name.to_string_lossy().contains(".awaken-file-stage-"))
            );
            drop(lock);
            temp.close().unwrap();
        }
    }
}
