//! Canonical Skill bundle ingestion shared by every transport adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use crate::SkillBundleFile;

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
