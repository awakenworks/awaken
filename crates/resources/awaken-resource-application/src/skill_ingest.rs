//! Canonical Skill bundle ingestion shared by every transport adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use awaken_resource_contract::SkillBundleFile;

/// Maximum number of regular files in one immutable Skill bundle.
pub const MAX_SKILL_FILES: usize = 128;
/// Maximum expanded size of one regular Skill file.
pub const MAX_SKILL_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum total expanded size of one Skill bundle.
pub const MAX_SKILL_BUNDLE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum size accepted for one multipart upload field or ZIP transport.
pub const MAX_SKILL_ARCHIVE_BYTES: usize = 8 * 1024 * 1024;

/// One untrusted file supplied by an import adapter. Folder upload and ZIP
/// decoding both converge on this transport-neutral value before persistence.
pub struct UploadedSkillBundleFile {
    pub path: String,
    pub content: Vec<u8>,
    pub executable: bool,
}

/// A validated bundle ready to become an immutable Skill version. The optional
/// source directory is transport metadata only; persisted paths are bundle-relative.
pub struct CanonicalSkillBundle {
    pub source_directory: Option<String>,
    pub files: Vec<SkillBundleFile>,
}

/// Normalize and validate one path at the Skill bundle trust boundary.
pub fn normalize_bundle_path(raw: &str) -> Result<String, String> {
    let path = raw.replace('\\', "/");
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(format!("invalid skill bundle path `{raw}`"));
    }
    Ok(path)
}

fn decode_zip(bytes: &[u8]) -> Result<Vec<UploadedSkillBundleFile>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("invalid skill ZIP: {error}"))?;
    if archive.len() > MAX_SKILL_FILES.saturating_mul(2) {
        return Err(format!("skill bundle exceeds {MAX_SKILL_FILES} files"));
    }
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("invalid skill ZIP entry: {error}"))?;
        if entry.is_dir() {
            continue;
        }
        let unix_mode = entry.unix_mode().unwrap_or_default();
        let file_type = unix_mode & 0o170_000;
        if file_type == 0o120_000 || (file_type != 0 && file_type != 0o100_000) {
            return Err(format!(
                "skill ZIP entry `{}` is not a regular file",
                entry.name()
            ));
        }
        if entry.size() > MAX_SKILL_FILE_BYTES as u64 {
            return Err(format!(
                "skill bundle file exceeds {MAX_SKILL_FILE_BYTES} bytes"
            ));
        }
        let path = entry.name().to_string();
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .take(MAX_SKILL_FILE_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .map_err(|error| format!("cannot read skill ZIP entry `{path}`: {error}"))?;
        if content.len() > MAX_SKILL_FILE_BYTES {
            return Err(format!(
                "skill bundle file exceeds {MAX_SKILL_FILE_BYTES} bytes"
            ));
        }
        files.push(UploadedSkillBundleFile {
            path,
            content,
            executable: unix_mode & 0o111 != 0,
        });
    }
    Ok(files)
}

/// Canonical import path shared by every Skill transport. It expands a sole ZIP,
/// strips at most one common directory, rejects aliases/unsafe paths, verifies
/// limits and UTF-8 `SKILL.md`, and preserves only the executable permission bit.
pub fn canonicalize_skill_bundle(
    mut files: Vec<UploadedSkillBundleFile>,
) -> Result<CanonicalSkillBundle, String> {
    if files.len() == 1 && files[0].path.to_ascii_lowercase().ends_with(".zip") {
        files = decode_zip(&files.remove(0).content)?;
    } else if files
        .iter()
        .any(|file| file.path.to_ascii_lowercase().ends_with(".zip"))
    {
        return Err("a Skill ZIP cannot be mixed with individual files".into());
    }
    if files.len() > MAX_SKILL_FILES {
        return Err(format!("skill bundle exceeds {MAX_SKILL_FILES} files"));
    }
    let mut total = 0usize;
    let mut normalized = BTreeMap::new();
    let mut folded_paths = BTreeSet::new();
    for file in files {
        let path = normalize_bundle_path(&file.path)?;
        if !folded_paths.insert(path.to_lowercase()) {
            return Err(format!(
                "duplicate skill bundle path after case normalization `{path}`"
            ));
        }
        if file.content.len() > MAX_SKILL_FILE_BYTES {
            return Err(format!(
                "skill bundle file exceeds {MAX_SKILL_FILE_BYTES} bytes"
            ));
        }
        total = total.saturating_add(file.content.len());
        if total > MAX_SKILL_BUNDLE_BYTES {
            return Err(format!(
                "skill bundle exceeds {MAX_SKILL_BUNDLE_BYTES} bytes"
            ));
        }
        if normalized.insert(path.clone(), file).is_some() {
            return Err(format!("duplicate skill bundle path `{path}`"));
        }
    }

    let (source_directory, prefix) = if normalized.contains_key("SKILL.md") {
        (None, None)
    } else {
        let roots = normalized
            .keys()
            .filter_map(|path| path.split('/').next())
            .collect::<BTreeSet<_>>();
        if roots.len() != 1 {
            return Err("skill files must share one top-level directory".into());
        }
        let root = roots.into_iter().next().expect("one root").to_string();
        let prefix = format!("{root}/");
        if !normalized.contains_key(&format!("{prefix}SKILL.md")) {
            return Err("skill upload has no SKILL.md at the bundle root".into());
        }
        (Some(root), Some(prefix))
    };

    let mut canonical = BTreeMap::new();
    let mut canonical_folded_paths = BTreeSet::new();
    for (path, file) in normalized {
        let path = prefix
            .as_ref()
            .and_then(|prefix| path.strip_prefix(prefix))
            .unwrap_or(&path)
            .to_string();
        if !canonical_folded_paths.insert(path.to_lowercase()) {
            return Err(format!(
                "duplicate skill bundle path after case normalization `{path}`"
            ));
        }
        let executable =
            file.executable || (path.starts_with("scripts/") && file.content.starts_with(b"#!"));
        if canonical
            .insert(
                path.clone(),
                SkillBundleFile {
                    path: path.clone(),
                    content: file.content,
                    executable,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate skill bundle path `{path}`"));
        }
    }
    let skill_md = canonical
        .get("SKILL.md")
        .ok_or_else(|| "skill upload has no SKILL.md at the bundle root".to_string())?;
    std::str::from_utf8(&skill_md.content)
        .map_err(|_| "SKILL.md must be valid UTF-8".to_string())?;
    Ok(CanonicalSkillBundle {
        source_directory,
        files: canonical.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn uploaded(path: &str, content: impl Into<Vec<u8>>) -> UploadedSkillBundleFile {
        UploadedSkillBundleFile {
            path: path.to_owned(),
            content: content.into(),
            executable: false,
        }
    }

    fn zip_file(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, content, mode) in entries {
            writer
                .start_file(
                    *path,
                    zip::write::SimpleFileOptions::default().unix_permissions(*mode),
                )
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// Skill-ingest FMECA / cause-effect graph:
    /// C1 input is loose files or one ZIP; C2 paths are safe and case-unique;
    /// C3 files share zero/one root containing UTF-8 SKILL.md; C4 expanded file,
    /// count and aggregate limits hold; C5 entry is regular. Effects are E1 one
    /// sorted, root-relative immutable bundle, or E2 fail closed before storage.
    /// F1 path traversal/alias (S9,O4,D2,RPN72), F2 ZIP special-file escape
    /// (S10,O3,D3,RPN90), F3 decompression/size exhaustion (S8,O5,D2,RPN80),
    /// and F4 ambiguous/malformed instructions (S7,O5,D2,RPN70) all map to E2.
    ///
    /// | Rule | form | safe/unique | root+UTF-8 | limits | regular | Effect |
    /// |---|---|---|---|---|---|---|
    /// | I1 | loose | yes | yes | yes | yes | E1 |
    /// | I2 | ZIP | yes | yes | yes | yes | E1 |
    /// | I3 | either | no | any | any | any | E2/F1 |
    /// | I4 | ZIP | yes | yes | yes | no | E2/F2 |
    /// | I5 | either | yes | yes | no | yes | E2/F3 |
    /// | I6 | either | yes | no | yes | yes | E2/F4 |
    #[test]
    fn canonical_ingest_decision_table() {
        let loose = canonicalize_skill_bundle(vec![
            uploaded("demo/SKILL.md", b"---\nname: demo\n---\n".to_vec()),
            uploaded("demo/scripts/run.sh", b"#!/bin/sh\nexit 0\n".to_vec()),
        ])
        .expect("I1");
        assert_eq!(loose.source_directory.as_deref(), Some("demo"), "I1/E1");
        assert_eq!(loose.files[0].path, "SKILL.md", "I1 sorted");
        assert_eq!(loose.files[1].path, "scripts/run.sh", "I1 relative");
        assert!(loose.files[1].executable, "I1 shebang capability");

        let zip = zip_file(&[("pkg/SKILL.md", b"valid", 0o100644)]);
        let zipped = canonicalize_skill_bundle(vec![uploaded("skill.zip", zip)]).expect("I2");
        assert_eq!(zipped.source_directory.as_deref(), Some("pkg"), "I2/E1");

        for files in [
            vec![uploaded("../SKILL.md", b"bad".to_vec())],
            vec![
                uploaded("SKILL.md", b"ok".to_vec()),
                uploaded("skill.md", b"alias".to_vec()),
            ],
            vec![
                uploaded("SKILL.md", b"ok".to_vec()),
                uploaded("extra.zip", b"not a zip".to_vec()),
            ],
        ] {
            assert!(canonicalize_skill_bundle(files).is_err(), "I3/E2/F1");
        }

        let mut special_writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        special_writer
            .start_file("pkg/SKILL.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        special_writer.write_all(b"valid").unwrap();
        special_writer
            .add_symlink(
                "pkg/link",
                "../outside",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        let special = special_writer.finish().unwrap().into_inner();
        assert!(
            canonicalize_skill_bundle(vec![uploaded("skill.zip", special)]).is_err(),
            "I4/E2/F2"
        );

        assert!(
            canonicalize_skill_bundle(vec![
                uploaded("SKILL.md", b"ok".to_vec()),
                uploaded("large.bin", vec![0; MAX_SKILL_FILE_BYTES + 1]),
            ])
            .is_err(),
            "I5/E2/F3"
        );

        for files in [
            vec![uploaded("other.txt", b"missing".to_vec())],
            vec![uploaded("SKILL.md", vec![0xff])],
            vec![
                uploaded("one/SKILL.md", b"one".to_vec()),
                uploaded("two/file", b"two".to_vec()),
            ],
        ] {
            assert!(canonicalize_skill_bundle(files).is_err(), "I6/E2/F4");
        }
    }
}
