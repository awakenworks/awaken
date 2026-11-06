//! T29-C03 a (产物来源与路径安全 — path boundary), native Local provider.
//!
//! Oracles covered here (groups-6.json order): O0 never escape the controlled
//! root or write an ancestor; O2 sources are not cleaned before collection is
//! complete (harvest is read-only and repeatable). O1 (Run provenance) belongs
//! to the realization harvest owner; O3 is in `git_transport` credential tests.
//!
//! | Row | Layout under the Sandbox root | Effect |
//! |---|---|---|
//! | P1 | long Unicode nested names, dot-names, a shared-prefix sibling `outputs-evil` | exactly the outputs files, by component |
//! | P2 | P1 plus a symlink/junction inside outputs to an outside directory | the whole harvest refuses; outside untouched |
//! | P3 | repeated collection | identical listing and bytes; nothing removed or written anywhere |
//! | P4 | Repository mount whose ancestor is a symlink/junction to outside | rejected; nothing published outside |
//! | NC | a host-string `starts_with` collector over the same tree | admits `outputs-evil`; the oracle rejects it |
use super::workdir_helper_tests::workdir_spec;
use super::*;
use pc::Sandbox as _;
use std::path::{Path, PathBuf};

const OUTPUTS: &str = "mnt/session/outputs";

/// Every non-directory entry beneath `root` as (relative path, bytes or link
/// marker), never following a link. Used to prove "nothing was written".
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                entries.push((relative, b"<link>".to_vec()));
            } else if metadata.is_dir() {
                entries.push((relative, b"<dir>".to_vec()));
                pending.push(path);
            } else {
                entries.push((relative, std::fs::read(&path).unwrap()));
            }
        }
    }
    entries.sort();
    entries
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// A directory link that needs no privilege: a junction on Windows.
fn link_directory(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        // `cmd` parses a `/` inside an argument as a switch, and `mklink`
        // does not accept verbatim (`\\?\`) paths: hand it native separators.
        fn native(path: &Path) -> String {
            let text = path.to_string_lossy().replace('/', "\\");
            text.strip_prefix(r"\\?\")
                .map(str::to_owned)
                .unwrap_or(text)
        }
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(native(link))
            .arg(native(target))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "create junction {}: {}{}",
            link.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn unlink_directory(link: &Path) {
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
}

/// O0 oracle: every collected sandbox path lies under the outputs root by
/// path component, not by string prefix.
fn inside_outputs(paths: &[String]) -> Result<(), String> {
    let root = Path::new("/").join(OUTPUTS);
    for path in paths {
        let candidate = Path::new(path);
        if !candidate.starts_with(&root)
            || candidate
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!("{path} escapes {}", root.display()));
        }
    }
    Ok(())
}

fn long_unicode_directory(outputs: &Path) -> PathBuf {
    let mut directory = outputs.to_owned();
    for level in 0..6 {
        directory.push(format!("層{level}-résumé-数据-ünïcødé-{}", "ł".repeat(12)));
    }
    directory
}

#[tokio::test]
async fn harvest_stays_inside_the_outputs_root_and_never_writes_or_cleans() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("provider");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
    let provider = LocalProvider::new(&base);
    let sandbox = provider
        .create_sandbox(&workdir_spec("t29-c03-paths", false))
        .await
        .unwrap();
    let root = sandbox.workspace_path().to_owned();
    let outputs = root.join(OUTPUTS);
    std::fs::create_dir_all(&outputs).unwrap();
    std::fs::write(tmp.path().join("ancestor.txt"), b"ancestor").unwrap();

    // P1 layout.
    let long = long_unicode_directory(&outputs);
    std::fs::create_dir_all(&long).unwrap();
    std::fs::write(long.join("深い.txt"), b"long unicode").unwrap();
    std::fs::write(outputs.join("a..b"), b"dot name").unwrap();
    std::fs::write(outputs.join(".hidden"), b"hidden").unwrap();
    let sibling = root.join("mnt/session/outputs-evil");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("secret.txt"), b"sibling secret").unwrap();

    // P2: a link inside outputs refuses the whole collection.
    let escape = outputs.join("escape");
    link_directory(&escape, &outside);
    let before_refusal = snapshot(tmp.path());
    assert!(
        sandbox.artifacts().await.is_err(),
        "P2/O0 a link inside outputs is never followed or published"
    );
    assert_eq!(
        snapshot(tmp.path()),
        before_refusal,
        "P2/O0 nothing written"
    );
    unlink_directory(&escape);

    // P1/P3: exact listing, repeatable, read-only.
    let before = snapshot(tmp.path());
    let first = sandbox.artifacts().await.unwrap();
    let paths = first.iter().map(|a| a.path.clone()).collect::<Vec<_>>();
    inside_outputs(&paths).unwrap_or_else(|violation| panic!("P1/O0: {violation}"));
    assert_eq!(first.len(), 3, "P1 exactly the outputs files: {paths:?}");
    assert!(
        paths.iter().all(|path| !path.contains("outputs-evil")),
        "P1/O0 shared-prefix sibling is not an output"
    );
    let long_artifact = first
        .iter()
        .find(|artifact| artifact.path.ends_with("/深い.txt"))
        .expect("P1 long Unicode output");
    assert_eq!(
        sandbox.read_artifact(&long_artifact.id).await.unwrap(),
        b"long unicode"
    );
    let second = sandbox.artifacts().await.unwrap();
    assert_eq!(second, first, "P3 repeated collection is identical");
    assert_eq!(
        snapshot(tmp.path()),
        before,
        "P3/O0/O2 collection removed nothing and wrote nowhere"
    );

    // NC: a host-string prefix collector over the same tree is detected.
    // Compare one separator spelling: `root.join("mnt/session/outputs")`
    // keeps `/` inside a `\` host path on Windows, so a raw string prefix
    // would match nothing and the NC would be vacuous.
    let host_string = |path: &Path| path.to_string_lossy().replace('\\', "/");
    let outputs_host = host_string(&outputs);
    let mut prefix_collected = Vec::new();
    for (relative, contents) in snapshot(&root.join("mnt/session")) {
        let host = root.join("mnt/session").join(&relative);
        if contents != b"<dir>" && host_string(&host).starts_with(&outputs_host) {
            let logical = host
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            prefix_collected.push(format!("/{logical}"));
        }
    }
    let violation = inside_outputs(&prefix_collected)
        .expect_err("NC a string-prefix boundary must be detected");
    assert!(violation.contains("outputs-evil"), "NC: {violation}");
}

#[tokio::test]
async fn repository_mount_through_a_linked_ancestor_never_publishes_outside() {
    // P4: the Repository destination's ancestor is a link to an outside
    // directory. The clone must be refused before anything is staged or
    // published through the link.
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    crate::git_transport::run_git(Some(&source), &["init", "-q"]).unwrap();
    std::fs::write(source.join("a.txt"), b"fixture").unwrap();
    crate::git_transport::run_git(Some(&source), &["add", "a.txt"]).unwrap();
    crate::git_transport::run_git(
        Some(&source),
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    )
    .unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    link_directory(&root.join("workspace"), &outside);
    let before = snapshot(&outside);
    let result = crate::git_transport::provision_repo_at(
        &crate::IsolatedRoot::new(&root),
        "workspace/repo",
        source.to_str().unwrap(),
        None,
        None,
        None,
        pc::RepositoryRealizationIntent::Initialize,
    );
    assert!(
        result.is_err(),
        "P4/O0 a linked ancestor must not admit the Repository mount"
    );
    assert_eq!(
        snapshot(&outside),
        before,
        "P4/O0 nothing staged or published outside the root"
    );
}
