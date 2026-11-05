//! Complete candidate Skill ingestion through the existing Resources authority.
//!
//! Trusted local synthetic evaluation supplies an exact File reference after
//! ordinary artifact publication. This adapter verifies bytes; it does not own
//! a Skill store, sandbox, Run, scorer, quality receipt or promotion. Candidate
//! output cannot supply a host path or construct the verified value.

use awaken_resource_application::{
    MAX_SKILL_ARCHIVE_BYTES, UploadedSkillBundleFile, canonicalize_skill_bundle,
};
use awaken_resource_contract::{
    FileContentSource, FileReadPurpose, SkillBundleFile, content_id, skill_bundle_sha256,
};

/// Immutable pins supplied by trusted orchestration, never an authoring tool DTO.
#[derive(Debug, Clone)]
pub struct SkillCandidateRef {
    workspace_id: String,
    file_id: String,
    content_id: String,
    archive_size: usize,
    bundle_sha256: String,
}

impl SkillCandidateRef {
    /// The complete ZIP is a logical File, not an optimizer-owned blob or a path.
    pub fn new(
        workspace_id: impl Into<String>,
        file_id: impl Into<String>,
        content_id: impl Into<String>,
        archive_size: usize,
        bundle_sha256: impl Into<String>,
    ) -> Result<Self, SkillCandidateError> {
        let reference = Self {
            workspace_id: workspace_id.into(),
            file_id: file_id.into(),
            content_id: content_id.into(),
            archive_size,
            bundle_sha256: bundle_sha256.into(),
        };
        if reference.workspace_id.trim().is_empty()
            || reference.file_id.trim().is_empty()
            || !is_digest(&reference.content_id)
            || !reference
                .bundle_sha256
                .strip_prefix("sha256:")
                .is_some_and(is_digest)
        {
            return Err(SkillCandidateError::InvalidReference);
        }
        if !(1..=MAX_SKILL_ARCHIVE_BYTES).contains(&archive_size) {
            return Err(SkillCandidateError::ArchiveBoundsRejected);
        }
        Ok(reference)
    }
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A complete canonical bundle with a verified frozen identity.
///
/// No deserializer or public constructor exists. Bytes and executable bits stay
/// exactly as Resources ingestion produced them; consumers cannot silently load
/// baseline references or drop support files while retaining this identity.
pub struct VerifiedSkillCandidate {
    reference: SkillCandidateRef,
    files: Vec<SkillBundleFile>,
}

impl VerifiedSkillCandidate {
    #[must_use]
    pub fn files(&self) -> &[SkillBundleFile] {
        &self.files
    }

    #[must_use]
    pub fn bundle_sha256(&self) -> &str {
        &self.reference.bundle_sha256
    }

    #[must_use]
    pub fn source_file_id(&self) -> &str {
        &self.reference.file_id
    }
}

impl std::fmt::Debug for VerifiedSkillCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedSkillCandidate")
            .field("file_id", &self.source_file_id())
            .field("bundle_sha256", &self.bundle_sha256())
            .field("file_count", &self.files.len())
            .finish()
    }
}

/// Content-free diagnostics. Resource/parser/ZIP excerpts must not reach reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkillCandidateError {
    #[error("candidate reference is invalid")]
    InvalidReference,
    #[error("candidate resource is unavailable")]
    ResourceUnavailable,
    #[error("candidate resource is missing")]
    ResourceMissing,
    #[error("candidate resource identity does not match its frozen pin")]
    ResourceIdentityMismatch,
    #[error("candidate archive exceeds its bounds")]
    ArchiveBoundsRejected,
    #[error("candidate Skill bundle was rejected by Resources ingestion")]
    BundleRejected,
    #[error("candidate Skill bundle does not match its frozen digest")]
    BundleDigestMismatch,
}

/// Reuse the scoped/fenced File read and canonical bundle importer unchanged.
///
/// The composition owns reference provenance, synthetic-content admission and
/// Session resource binding. A known id/hash is not read authority. The supplied
/// source must enforce current Workspace/reference/claim authorization, including
/// its asynchronous read boundary, exactly as for ordinary Session resources.
/// This call never opens a filesystem path or appends a visible Skill version.
pub async fn load_skill_candidate<C: Sync>(
    source: &dyn FileContentSource<C>,
    reference: &SkillCandidateRef,
    fence: &C,
) -> Result<VerifiedSkillCandidate, SkillCandidateError> {
    let content = source
        .read(
            &reference.workspace_id,
            &reference.file_id,
            &FileReadPurpose::SessionResource {
                reader_session_id: None,
            },
            Some(fence),
        )
        .await
        .map_err(|_| SkillCandidateError::ResourceUnavailable)?
        .ok_or(SkillCandidateError::ResourceMissing)?;
    if content.bytes.len() > MAX_SKILL_ARCHIVE_BYTES {
        return Err(SkillCandidateError::ArchiveBoundsRejected);
    }
    if content.file_id != reference.file_id
        || content.content_id != reference.content_id
        || content.bytes.len() != reference.archive_size
        || content_id(&content.bytes) != reference.content_id
    {
        return Err(SkillCandidateError::ResourceIdentityMismatch);
    }
    // The transport is closed to a complete archive. Never use a provider- or
    // candidate-supplied filename as a local path or an executable command.
    let bundle = canonicalize_skill_bundle(vec![UploadedSkillBundleFile {
        path: "candidate.zip".into(),
        content: content.bytes,
        executable: false,
    }])
    .map_err(|_| SkillCandidateError::BundleRejected)?;
    if skill_bundle_sha256(&bundle.files) != reference.bundle_sha256 {
        return Err(SkillCandidateError::BundleDigestMismatch);
    }
    Ok(VerifiedSkillCandidate {
        reference: reference.clone(),
        files: bundle.files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    use awaken_resource_contract::{FileContentSourceError, ResolvedFileContent};

    struct Source(Result<Option<ResolvedFileContent>, FileContentSourceError>);

    #[async_trait::async_trait]
    impl FileContentSource<u64> for Source {
        async fn read(
            &self,
            workspace: &str,
            file: &str,
            purpose: &FileReadPurpose,
            fence: Option<&u64>,
        ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
            assert_eq!(workspace, "evaluation-workspace");
            assert_eq!(file, "candidate-file");
            assert_eq!(
                purpose,
                &FileReadPurpose::SessionResource {
                    reader_session_id: None
                }
            );
            assert_eq!(
                fence,
                Some(&7),
                "the caller's exact fence reaches Resources"
            );
            self.0.clone()
        }
    }

    fn archive(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        // These fixtures model POSIX modes, independently of the writer's OS.
        archive_with_system(files, zip::System::Unix)
    }

    fn archive_with_system(files: &[(&str, &[u8], u32)], system: zip::System) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, bytes, mode) in files {
            writer
                .start_file(
                    *path,
                    zip::write::SimpleFileOptions::default()
                        .system(system)
                        .unix_permissions(*mode),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn frozen(bytes: Vec<u8>, bundle_hash: String) -> (SkillCandidateRef, ResolvedFileContent) {
        let digest = content_id(&bytes);
        let reference = SkillCandidateRef::new(
            "evaluation-workspace",
            "candidate-file",
            &digest,
            bytes.len(),
            bundle_hash,
        )
        .unwrap();
        let content = ResolvedFileContent {
            file_id: "candidate-file".into(),
            content_id: digest,
            filename: "../untrusted-provider-name.zip".into(),
            media_type: "application/zip".into(),
            bytes,
        };
        (reference, content)
    }

    fn fixture() -> (SkillCandidateRef, ResolvedFileContent, Vec<SkillBundleFile>) {
        let files = vec![
            SkillBundleFile {
                path: "SKILL.md".into(),
                content: b"---\nname: fixture\ndescription: synthetic fixture\n---\n".to_vec(),
                executable: false,
            },
            SkillBundleFile {
                path: "references/rules.md".into(),
                content: b"PRIVATE-BUNDLE-CONTENT".to_vec(),
                executable: false,
            },
            SkillBundleFile {
                path: "scripts/check.sh".into(),
                content: b"exit 0\n".to_vec(),
                executable: true,
            },
        ];
        let bytes = archive(
            &files
                .iter()
                .map(|file| {
                    (
                        file.path.as_str(),
                        file.content.as_slice(),
                        if file.executable { 0o755 } else { 0o644 },
                    )
                })
                .collect::<Vec<_>>(),
        );
        let (reference, content) = frozen(bytes, skill_bundle_sha256(&files));
        (reference, content, files)
    }

    // Candidate ingestion cause/effect graph:
    // C1 trusted reference is well-formed/bounded; C2 scoped/fenced source is
    // available; C3 returned logical identity/size/bytes equal the pin; C4 the
    // existing Resources canonicalizer admits the complete archive; C5 the
    // complete bundle digest (paths+bytes+executable bits) equals the frozen pin.
    // E1 immutable complete candidate, or E2 fixed refusal before execution.
    // Decision rules: V1 all C -> E1; V2 !C1 -> no read; V3 !C2 -> unavailable/
    // missing, not quality failure; V4 !C3 -> identity rejection; V5 !C4 ->
    // admission rejection; V6 !C5 -> digest rejection. No row creates a Skill
    // version or an execution/quality receipt, and diagnostics/debug omit bytes.
    #[tokio::test]
    async fn exact_candidate_preserves_support_files_and_permissions() {
        let (reference, content, files) = fixture();
        let candidate = load_skill_candidate(&Source(Ok(Some(content))), &reference, &7)
            .await
            .unwrap();
        assert_eq!(candidate.files(), files, "V1 complete canonical bundle");
        assert_eq!(candidate.bundle_sha256(), skill_bundle_sha256(&files));
        assert_eq!(candidate.source_file_id(), "candidate-file");
        assert!(!format!("{candidate:?}").contains("PRIVATE-BUNDLE-CONTENT"));

        // ZIP 9 uses the host's creator system unless explicitly selected.
        // Cross creator/mode with script/reference and shebang/ordinary bytes:
        // Unix 755 supplies execution, DOS mode bits do not; only scripts gain
        // execution from a shebang. Every row must load its exact complete
        // bundle and reject a pin differing solely in its executable bit.
        // These sixteen positive/negative pairs use the real Resources parser,
        // not filesystem permissions or another ZIP interpretation.
        for (system, mode, mode_exec) in [
            (zip::System::Unix, 0o644, false),
            (zip::System::Unix, 0o755, true),
            (zip::System::Dos, 0o644, false),
            (zip::System::Dos, 0o755, false),
        ] {
            for (path, bytes, inferred_exec) in [
                ("scripts/run.sh", &b"exit 0\n"[..], false),
                ("scripts/run.sh", &b"#!/bin/sh\nexit 0\n"[..], true),
                ("references/run.sh", &b"exit 0\n"[..], false),
                ("references/run.sh", &b"#!/bin/sh\nexit 0\n"[..], false),
            ] {
                let expected = vec![
                    SkillBundleFile {
                        path: "SKILL.md".into(),
                        content: b"valid".to_vec(),
                        executable: false,
                    },
                    SkillBundleFile {
                        path: path.into(),
                        content: bytes.to_vec(),
                        executable: mode_exec || inferred_exec,
                    },
                ];
                let archive = archive_with_system(
                    &[("SKILL.md", &b"valid"[..], 0o644), (path, bytes, mode)],
                    system,
                );
                let (reference, content) = frozen(archive, skill_bundle_sha256(&expected));
                let source = Source(Ok(Some(content)));
                let candidate = load_skill_candidate(&source, &reference, &7)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("V1 creator={system:?} mode={mode:o} path={path}: {error}")
                    });
                assert_eq!(candidate.files(), expected, "V1 complete permission frame");

                let mut wrong_files = expected;
                wrong_files[1].executable = !wrong_files[1].executable;
                let mut wrong_pin = reference;
                wrong_pin.bundle_sha256 = skill_bundle_sha256(&wrong_files);
                assert_eq!(
                    load_skill_candidate(&source, &wrong_pin, &7)
                        .await
                        .unwrap_err(),
                    SkillCandidateError::BundleDigestMismatch,
                    "V6 executable bit is part of the exact pin"
                );
            }
        }
    }

    #[test]
    fn reference_admission_rejects_invalid_and_unbounded_pins() {
        // V2: validation occurs when constructing the trusted reference, before
        // any FileContentSource call; closed identities do not encode host paths.
        let digest = "a".repeat(64);
        let bundle = format!("sha256:{digest}");
        for (workspace, file, hash, size, skill_hash, expected) in [
            (
                "",
                "candidate-file",
                digest.as_str(),
                1,
                bundle.as_str(),
                SkillCandidateError::InvalidReference,
            ),
            (
                "evaluation-workspace",
                "",
                digest.as_str(),
                1,
                bundle.as_str(),
                SkillCandidateError::InvalidReference,
            ),
            (
                "evaluation-workspace",
                "candidate-file",
                "not-a-digest",
                1,
                bundle.as_str(),
                SkillCandidateError::InvalidReference,
            ),
            (
                "evaluation-workspace",
                "candidate-file",
                digest.as_str(),
                1,
                "SHA256:private-content",
                SkillCandidateError::InvalidReference,
            ),
            (
                "evaluation-workspace",
                "candidate-file",
                digest.as_str(),
                0,
                bundle.as_str(),
                SkillCandidateError::ArchiveBoundsRejected,
            ),
            (
                "evaluation-workspace",
                "candidate-file",
                digest.as_str(),
                MAX_SKILL_ARCHIVE_BYTES + 1,
                bundle.as_str(),
                SkillCandidateError::ArchiveBoundsRejected,
            ),
        ] {
            assert_eq!(
                SkillCandidateRef::new(workspace, file, hash, size, skill_hash).unwrap_err(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn resource_failures_and_identity_drift_never_yield_candidates() {
        let (reference, content, _) = fixture();
        // V3: do not copy transport diagnostics (which may contain credentials,
        // provider data or private paths) into the public fixed error.
        for (source, expected) in [
            (
                Source(Err(FileContentSourceError::new(
                    "PRIVATE-PROVIDER-DIAGNOSTIC",
                ))),
                SkillCandidateError::ResourceUnavailable,
            ),
            (Source(Ok(None)), SkillCandidateError::ResourceMissing),
        ] {
            assert_eq!(
                load_skill_candidate(&source, &reference, &7)
                    .await
                    .unwrap_err(),
                expected
            );
        }
        // V4: test each independent identity axis; metadata alone cannot vouch
        // for changed bytes, including same-length corruption.
        for axis in 0..5 {
            let mut changed = content.clone();
            match axis {
                0 => changed.file_id = "foreign-file".into(),
                1 => changed.content_id = "b".repeat(64),
                2 => changed.bytes.push(0),
                3 => changed.bytes[0] ^= 1,
                _ => changed.bytes.resize(MAX_SKILL_ARCHIVE_BYTES + 1, 0),
            }
            let expected = if axis == 4 {
                SkillCandidateError::ArchiveBoundsRejected
            } else {
                SkillCandidateError::ResourceIdentityMismatch
            };
            assert_eq!(
                load_skill_candidate(&Source(Ok(Some(changed))), &reference, &7)
                    .await
                    .unwrap_err(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn resources_admission_and_full_bundle_digest_are_hard_gates() {
        // V5: no alternate ZIP/path parser. Existing ingestion rejects missing
        // instructions, traversal, case aliases and invalid UTF-8 instructions.
        for bytes in [
            b"PRIVATE-MALFORMED-ARCHIVE".to_vec(),
            archive(&[("references/only.md", b"no instructions", 0o644)]),
            archive(&[("../SKILL.md", b"unsafe", 0o644)]),
            archive(&[("SKILL.md", b"safe", 0o644), ("skill.md", b"alias", 0o644)]),
            archive(&[("SKILL.md", &[0xff], 0o644)]),
        ] {
            let (reference, content) = frozen(bytes, format!("sha256:{}", "a".repeat(64)));
            let error = load_skill_candidate(&Source(Ok(Some(content))), &reference, &7)
                .await
                .unwrap_err();
            assert_eq!(error, SkillCandidateError::BundleRejected);
            assert!(!error.to_string().contains("PRIVATE"));
        }
        // V6: changing a support reference or executable bit with the old bundle
        // pin must fail even when the ZIP's own content identity is current.
        let (reference, _, original_files) = fixture();
        for axis in 0..4 {
            let mut files = original_files.clone();
            match axis {
                0 => files[1].content.push(b'!'),
                1 => files[2].executable = false,
                2 => files[1].path = "references/other.md".into(),
                _ => {
                    files.remove(1);
                }
            }
            let bytes = archive(
                &files
                    .iter()
                    .map(|file| {
                        (
                            file.path.as_str(),
                            file.content.as_slice(),
                            if file.executable { 0o755 } else { 0o644 },
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            let (changed_reference, changed_content) =
                frozen(bytes, reference.bundle_sha256.clone());
            assert_eq!(
                load_skill_candidate(&Source(Ok(Some(changed_content))), &changed_reference, &7)
                    .await
                    .unwrap_err(),
                SkillCandidateError::BundleDigestMismatch
            );
        }
    }
}
