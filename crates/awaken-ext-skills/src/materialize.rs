use crate::skill::{LoadedAsset, LoadedReference, ScriptResult};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::error::SkillMaterializeError;

const MAX_REFERENCE_BYTES: usize = 256 * 1024;
const MAX_ASSET_BYTES: usize = 512 * 1024;
const MAX_STDOUT_BYTES: usize = 32 * 1024;
const MAX_STDERR_BYTES: usize = 32 * 1024;
const MAX_SCRIPT_ARGS: usize = 64;
const MAX_SCRIPT_ARG_BYTES: usize = 8 * 1024;
const MAX_SCRIPT_ARGS_TOTAL_BYTES: usize = 64 * 1024;
const SCRIPT_TIMEOUT_SECS: u64 = 60;

pub(crate) fn load_reference_material(
    skill_id: &str,
    skill_root: &Path,
    relative_path: &str,
) -> Result<LoadedReference, SkillMaterializeError> {
    let rel = normalize_relative_path(relative_path)?;
    require_prefix(&rel, "references")?;
    let full = resolve_under_root(skill_root, &rel)?;
    ensure_regular_file(&full)?;

    let bytes = std::fs::read(&full).map_err(|e| SkillMaterializeError::Io(e.to_string()))?;
    materialize_reference_bytes(skill_id, &rel.to_string_lossy(), &bytes)
}

pub(crate) fn load_asset_material(
    skill_id: &str,
    skill_root: &Path,
    relative_path: &str,
) -> Result<LoadedAsset, SkillMaterializeError> {
    let rel = normalize_relative_path(relative_path)?;
    require_prefix(&rel, "assets")?;
    let full = resolve_under_root(skill_root, &rel)?;
    ensure_regular_file(&full)?;

    let bytes = std::fs::read(&full).map_err(|e| SkillMaterializeError::Io(e.to_string()))?;
    materialize_asset_bytes(skill_id, &rel.to_string_lossy(), &bytes, None)
}

pub(crate) fn materialize_reference_bytes(
    skill_id: &str,
    relative_path: &str,
    bytes: &[u8],
) -> Result<LoadedReference, SkillMaterializeError> {
    let path = normalize_material_path(relative_path, "references")?;
    let original_len = bytes.len() as u64;

    let (stored_bytes, truncated) = if bytes.len() > MAX_REFERENCE_BYTES {
        (bytes[..MAX_REFERENCE_BYTES].to_vec(), true)
    } else {
        (bytes.to_vec(), false)
    };

    let content = String::from_utf8(stored_bytes.clone())
        .map_err(|e| SkillMaterializeError::Io(format!("invalid utf-8: {e}")))?;
    let sha256 = sha256_hex(&stored_bytes);

    debug!(
        skill_id = %skill_id,
        path = %path,
        bytes = original_len,
        truncated = truncated,
        "loaded skill reference material"
    );

    Ok(LoadedReference {
        skill: skill_id.to_string(),
        path,
        sha256,
        truncated,
        content,
        bytes: original_len,
    })
}

pub(crate) fn materialize_asset_bytes(
    skill_id: &str,
    relative_path: &str,
    bytes: &[u8],
    media_type: Option<String>,
) -> Result<LoadedAsset, SkillMaterializeError> {
    let path = normalize_material_path(relative_path, "assets")?;
    let original_len = bytes.len() as u64;

    let (stored_bytes, truncated) = if bytes.len() > MAX_ASSET_BYTES {
        (bytes[..MAX_ASSET_BYTES].to_vec(), true)
    } else {
        (bytes.to_vec(), false)
    };

    let sha256 = sha256_hex(&stored_bytes);
    let content = BASE64.encode(&stored_bytes);
    let media_type = media_type.or_else(|| infer_media_type(Path::new(&path)));

    debug!(
        skill_id = %skill_id,
        path = %path,
        bytes = original_len,
        truncated = truncated,
        media_type = media_type.as_deref().unwrap_or("application/octet-stream"),
        "loaded skill asset material"
    );

    Ok(LoadedAsset {
        skill: skill_id.to_string(),
        path,
        sha256,
        truncated,
        bytes: original_len,
        media_type,
        encoding: "base64".to_string(),
        content,
    })
}

fn normalize_material_path(
    relative_path: &str,
    required_prefix: &str,
) -> Result<String, SkillMaterializeError> {
    if relative_path.contains("://") {
        let prefix = format!("{required_prefix}/");
        if !relative_path.starts_with(&prefix) {
            return Err(SkillMaterializeError::UnsupportedPath(
                required_prefix.to_string(),
            ));
        }
        if relative_path[prefix.len()..].trim().is_empty() {
            return Err(SkillMaterializeError::InvalidPath("empty".to_string()));
        }
        return Ok(relative_path.to_string());
    }

    let rel = normalize_relative_path(relative_path)?;
    require_prefix(&rel, required_prefix)?;
    Ok(rel.to_string_lossy().to_string())
}

pub(crate) async fn run_script_material(
    skill_id: &str,
    skill_root: &Path,
    script_path: &str,
    args: &[String],
) -> Result<ScriptResult, SkillMaterializeError> {
    let rel = normalize_relative_path(script_path)?;
    require_prefix(&rel, "scripts")?;
    let full = resolve_under_root(skill_root, &rel)?;
    ensure_regular_file(&full)?;
    validate_script_args(args)?;

    let (program, mut cmd_args) = runtime_for_script(&full)?;
    cmd_args.push(full.to_string_lossy().to_string());
    cmd_args.extend(args.iter().cloned());

    let mut cmd = Command::new(program);
    cmd.args(cmd_args);
    cmd.current_dir(skill_root);
    cmd.kill_on_drop(true);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(SCRIPT_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .map_err(|_| SkillMaterializeError::Timeout(SCRIPT_TIMEOUT_SECS))?
    .map_err(|e| SkillMaterializeError::Io(e.to_string()))?;

    let exit_code = out.status.code().unwrap_or(-1);

    let (stdout, truncated_stdout) = truncate_bytes(&out.stdout, MAX_STDOUT_BYTES);
    let (stderr, truncated_stderr) = truncate_bytes(&out.stderr, MAX_STDERR_BYTES);

    let mut hasher = Sha256::new();
    hasher.update(&out.stdout);
    hasher.update(&out.stderr);
    let sha256 = hex_encode(&hasher.finalize());

    if truncated_stdout || truncated_stderr {
        warn!(
            skill_id = %skill_id,
            script = %rel.to_string_lossy(),
            truncated_stdout = truncated_stdout,
            truncated_stderr = truncated_stderr,
            "skill script output truncated"
        );
    }

    debug!(
        skill_id = %skill_id,
        script = %rel.to_string_lossy(),
        exit_code = exit_code,
        stdout_bytes = out.stdout.len(),
        stderr_bytes = out.stderr.len(),
        "executed skill script material"
    );

    Ok(ScriptResult {
        skill: skill_id.to_string(),
        script: rel.to_string_lossy().to_string(),
        sha256,
        truncated_stdout,
        truncated_stderr,
        exit_code,
        stdout,
        stderr,
    })
}

fn runtime_for_script(script: &Path) -> Result<(String, Vec<String>), SkillMaterializeError> {
    let ext = script.extension().and_then(|s| s.to_str()).unwrap_or("");
    match ext {
        "sh" => Ok(("bash".to_string(), vec![])),
        "py" => Ok(("python3".to_string(), vec![])),
        "js" => Ok(("node".to_string(), vec![])),
        _ => Err(SkillMaterializeError::UnsupportedRuntime(
            script.to_string_lossy().to_string(),
        )),
    }
}

fn infer_media_type(path: &Path) -> Option<String> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    let media = match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "yaml" | "yml" => "application/yaml",
        _ => return None,
    };
    Some(media.to_string())
}

fn validate_script_args(args: &[String]) -> Result<(), SkillMaterializeError> {
    if args.len() > MAX_SCRIPT_ARGS {
        return Err(SkillMaterializeError::InvalidScriptArgs(format!(
            "too many arguments (max {MAX_SCRIPT_ARGS})"
        )));
    }

    let mut total = 0usize;
    for (idx, arg) in args.iter().enumerate() {
        if arg.contains('\0') {
            return Err(SkillMaterializeError::InvalidScriptArgs(format!(
                "argument {idx} contains NUL byte"
            )));
        }
        let len = arg.len();
        if len > MAX_SCRIPT_ARG_BYTES {
            return Err(SkillMaterializeError::InvalidScriptArgs(format!(
                "argument {idx} exceeds {MAX_SCRIPT_ARG_BYTES} bytes"
            )));
        }
        total += len;
        if total > MAX_SCRIPT_ARGS_TOTAL_BYTES {
            return Err(SkillMaterializeError::InvalidScriptArgs(format!(
                "total argument size exceeds {MAX_SCRIPT_ARGS_TOTAL_BYTES} bytes"
            )));
        }
    }

    Ok(())
}

fn truncate_bytes(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).to_string(), false);
    }
    let truncated = &bytes[..max];
    (String::from_utf8_lossy(truncated).to_string(), true)
}

pub(crate) fn normalize_relative_path(p: &str) -> Result<PathBuf, SkillMaterializeError> {
    if p.trim().is_empty() {
        return Err(SkillMaterializeError::InvalidPath("empty".to_string()));
    }
    let raw = Path::new(p);
    if raw.is_absolute() {
        return Err(SkillMaterializeError::InvalidPath("absolute".to_string()));
    }
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                return Err(SkillMaterializeError::InvalidPath("absolute".to_string()));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(SkillMaterializeError::InvalidPath("parent_dir".to_string()));
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(SkillMaterializeError::InvalidPath("empty".to_string()));
    }
    Ok(out)
}

fn require_prefix(rel: &Path, required_first_component: &str) -> Result<(), SkillMaterializeError> {
    let first = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("");
    if first != required_first_component {
        return Err(SkillMaterializeError::UnsupportedPath(
            required_first_component.to_string(),
        ));
    }
    Ok(())
}

fn resolve_under_root(root: &Path, rel: &Path) -> Result<PathBuf, SkillMaterializeError> {
    let root = std::fs::canonicalize(root).map_err(|e| SkillMaterializeError::Io(e.to_string()))?;
    let joined = root.join(rel);
    let canon =
        std::fs::canonicalize(&joined).map_err(|e| SkillMaterializeError::Io(e.to_string()))?;
    if !canon.starts_with(&root) {
        return Err(SkillMaterializeError::PathEscapesRoot);
    }
    Ok(canon)
}

fn ensure_regular_file(path: &Path) -> Result<(), SkillMaterializeError> {
    let meta = std::fs::metadata(path).map_err(|e| SkillMaterializeError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(SkillMaterializeError::Io(format!(
            "path is not a regular file: {}",
            path.to_string_lossy()
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_reference_requires_references_prefix() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("docx").join("references")).unwrap();
        fs::write(
            td.path().join("docx").join("references").join("a.md"),
            "hello",
        )
        .unwrap();

        let err = load_reference_material("docx", &td.path().join("docx"), "a.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected under references"));
    }

    #[test]
    fn load_reference_allows_nested_paths() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("docx").join("references").join("nested")).unwrap();
        fs::write(
            td.path()
                .join("docx")
                .join("references")
                .join("nested")
                .join("a.md"),
            "hello",
        )
        .unwrap();

        let mat =
            load_reference_material("docx", &td.path().join("docx"), "references/nested/a.md")
                .expect("nested references are allowed");
        assert_eq!(mat.path, "references/nested/a.md");
        assert_eq!(mat.content.trim(), "hello");
    }

    #[test]
    fn load_asset_encodes_binary_payload_as_base64() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("docx").join("assets")).unwrap();
        fs::write(
            td.path().join("docx").join("assets").join("logo.png"),
            vec![0x89, 0x50, 0x4e, 0x47],
        )
        .unwrap();

        let mat = load_asset_material("docx", &td.path().join("docx"), "assets/logo.png")
            .expect("asset load should succeed");
        assert_eq!(mat.path, "assets/logo.png");
        assert_eq!(mat.encoding, "base64");
        assert_eq!(mat.content, "iVBORw==");
        assert_eq!(mat.media_type.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn run_script_rejects_path_escape() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("s").join("scripts")).unwrap();
        let err = run_script_material("s", &td.path().join("s"), "../x.sh", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid relative path"));
    }

    #[tokio::test]
    async fn run_script_rejects_too_many_arguments() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("s").join("scripts")).unwrap();
        fs::write(
            td.path().join("s").join("scripts").join("hello.sh"),
            "#!/usr/bin/env bash\necho hi\n",
        )
        .unwrap();

        let args = vec!["x".to_string(); MAX_SCRIPT_ARGS + 1];
        let err = run_script_material("s", &td.path().join("s"), "scripts/hello.sh", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid script arguments"));
    }

    // --- materialize_reference_bytes ---

    #[test]
    fn reference_bytes_basic() {
        let bytes = b"hello world";
        let r = materialize_reference_bytes("s1", "references/doc.md", bytes).unwrap();
        assert_eq!(r.skill, "s1");
        assert_eq!(r.path, "references/doc.md");
        assert_eq!(r.content, "hello world");
        assert!(!r.truncated);
        assert_eq!(r.bytes, 11);
        assert!(!r.sha256.is_empty());
    }

    #[test]
    fn reference_bytes_truncates_oversized() {
        let bytes = vec![b'A'; MAX_REFERENCE_BYTES + 100];
        let r = materialize_reference_bytes("s1", "references/big.txt", &bytes).unwrap();
        assert!(r.truncated);
        assert_eq!(r.content.len(), MAX_REFERENCE_BYTES);
        assert_eq!(r.bytes, (MAX_REFERENCE_BYTES + 100) as u64);
    }

    #[test]
    fn reference_bytes_rejects_non_utf8() {
        let bytes = vec![0xFF, 0xFE, 0x80];
        let err = materialize_reference_bytes("s1", "references/bin.dat", &bytes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("utf-8"));
    }

    #[test]
    fn reference_bytes_rejects_wrong_prefix() {
        let err = materialize_reference_bytes("s1", "assets/doc.md", b"hi")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected under references"));
    }

    // --- materialize_asset_bytes ---

    #[test]
    fn asset_bytes_basic() {
        let bytes = vec![0x89, 0x50, 0x4e, 0x47];
        let a = materialize_asset_bytes("s1", "assets/logo.png", &bytes, None).unwrap();
        assert_eq!(a.skill, "s1");
        assert_eq!(a.path, "assets/logo.png");
        assert_eq!(a.encoding, "base64");
        assert!(!a.truncated);
        assert_eq!(a.bytes, 4);
        assert_eq!(a.media_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn asset_bytes_truncates_oversized() {
        let bytes = vec![0x42; MAX_ASSET_BYTES + 200];
        let a = materialize_asset_bytes("s1", "assets/big.bin", &bytes, None).unwrap();
        assert!(a.truncated);
        assert_eq!(a.bytes, (MAX_ASSET_BYTES + 200) as u64);
    }

    #[test]
    fn asset_bytes_uses_explicit_media_type() {
        let a = materialize_asset_bytes(
            "s1",
            "assets/x.bin",
            b"data",
            Some("application/custom".to_string()),
        )
        .unwrap();
        assert_eq!(a.media_type.as_deref(), Some("application/custom"));
    }

    #[test]
    fn asset_bytes_rejects_wrong_prefix() {
        let err = materialize_asset_bytes("s1", "references/x.png", b"data", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected under assets"));
    }

    // --- normalize_relative_path ---

    #[test]
    fn normalize_rejects_empty() {
        let err = normalize_relative_path("").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn normalize_rejects_whitespace_only() {
        let err = normalize_relative_path("   ").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn normalize_rejects_absolute() {
        let err = normalize_relative_path("/etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"));
    }

    #[test]
    fn normalize_rejects_parent_dir() {
        let err = normalize_relative_path("../secret")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parent_dir"));
    }

    #[test]
    fn normalize_strips_curdir() {
        let p = normalize_relative_path("./references/doc.md").unwrap();
        assert_eq!(p.to_string_lossy(), "references/doc.md");
    }

    #[test]
    fn normalize_rejects_dot_only() {
        let err = normalize_relative_path(".").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    // --- normalize_material_path ---

    #[test]
    fn material_path_with_url_scheme_valid() {
        // The URL check triggers on "://" anywhere in the path, then checks prefix is "references/"
        let p = normalize_material_path("references/http://example.com/doc", "references").unwrap();
        assert_eq!(p, "references/http://example.com/doc");
    }

    #[test]
    fn material_path_with_url_scheme_wrong_prefix() {
        let err = normalize_material_path("assets/http://example.com", "references")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected under references"));
    }

    #[test]
    fn material_path_non_url_rejects_empty_after_normalize() {
        let err = normalize_material_path("", "references")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"));
    }

    // --- validate_script_args ---

    #[test]
    fn script_args_rejects_nul_byte() {
        let args = vec!["hello\0world".to_string()];
        let err = validate_script_args(&args).unwrap_err().to_string();
        assert!(err.contains("NUL byte"));
    }

    #[test]
    fn script_args_rejects_single_oversized_arg() {
        let args = vec!["x".repeat(MAX_SCRIPT_ARG_BYTES + 1)];
        let err = validate_script_args(&args).unwrap_err().to_string();
        assert!(err.contains(&format!("exceeds {MAX_SCRIPT_ARG_BYTES}")));
    }

    #[test]
    fn script_args_rejects_total_oversized() {
        // Each arg is just under the single-arg limit, but total exceeds
        let arg_size = MAX_SCRIPT_ARG_BYTES;
        let num_args = (MAX_SCRIPT_ARGS_TOTAL_BYTES / arg_size) + 1;
        let args: Vec<String> = (0..num_args).map(|_| "x".repeat(arg_size)).collect();
        let err = validate_script_args(&args).unwrap_err().to_string();
        assert!(err.contains("total argument size"));
    }

    #[test]
    fn script_args_accepts_valid() {
        let args = vec!["--flag".to_string(), "value".to_string()];
        assert!(validate_script_args(&args).is_ok());
    }

    // --- runtime_for_script ---

    #[test]
    fn runtime_for_sh_script() {
        let (prog, _) = runtime_for_script(Path::new("test.sh")).unwrap();
        assert_eq!(prog, "bash");
    }

    #[test]
    fn runtime_for_py_script() {
        let (prog, _) = runtime_for_script(Path::new("test.py")).unwrap();
        assert_eq!(prog, "python3");
    }

    #[test]
    fn runtime_for_js_script() {
        let (prog, _) = runtime_for_script(Path::new("test.js")).unwrap();
        assert_eq!(prog, "node");
    }

    #[test]
    fn runtime_for_unsupported_extension() {
        let err = runtime_for_script(Path::new("test.rb"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime not supported"));
    }

    #[test]
    fn runtime_for_no_extension() {
        let err = runtime_for_script(Path::new("script"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime not supported"));
    }

    // --- infer_media_type ---

    #[test]
    fn infer_media_type_common_formats() {
        assert_eq!(
            infer_media_type(Path::new("x.png")),
            Some("image/png".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.jpg")),
            Some("image/jpeg".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.jpeg")),
            Some("image/jpeg".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.gif")),
            Some("image/gif".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.webp")),
            Some("image/webp".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.svg")),
            Some("image/svg+xml".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.pdf")),
            Some("application/pdf".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.json")),
            Some("application/json".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.md")),
            Some("text/markdown".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.txt")),
            Some("text/plain".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.csv")),
            Some("text/csv".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.yaml")),
            Some("application/yaml".to_string())
        );
        assert_eq!(
            infer_media_type(Path::new("x.yml")),
            Some("application/yaml".to_string())
        );
    }

    #[test]
    fn infer_media_type_unknown_extension() {
        assert_eq!(infer_media_type(Path::new("x.xyz")), None);
    }

    #[test]
    fn infer_media_type_no_extension() {
        assert_eq!(infer_media_type(Path::new("Makefile")), None);
    }

    // --- truncate_bytes ---

    #[test]
    fn truncate_bytes_no_truncation() {
        let (s, t) = truncate_bytes(b"hello", 10);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_bytes_exact_limit() {
        let (s, t) = truncate_bytes(b"hello", 5);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_bytes_over_limit() {
        let (s, t) = truncate_bytes(b"hello world", 5);
        assert_eq!(s, "hello");
        assert!(t);
    }

    // --- load_reference_material with filesystem ---

    #[test]
    fn load_reference_rejects_path_with_parent_dir() {
        let td = TempDir::new().unwrap();
        let err = load_reference_material("s1", td.path(), "../references/x.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parent_dir"));
    }

    #[test]
    fn load_asset_rejects_non_asset_prefix() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("references")).unwrap();
        fs::write(td.path().join("references").join("x.md"), "hi").unwrap();
        let err = load_asset_material("s1", td.path(), "references/x.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected under assets"));
    }

    #[test]
    fn load_reference_nonexistent_file() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("references")).unwrap();
        let err = load_reference_material("s1", td.path(), "references/missing.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("io error") || err.contains("No such file"));
    }

    #[test]
    fn load_asset_nonexistent_file() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("assets")).unwrap();
        let err = load_asset_material("s1", td.path(), "assets/missing.png")
            .unwrap_err()
            .to_string();
        assert!(err.contains("io error") || err.contains("No such file"));
    }

    // --- require_prefix ---

    #[test]
    fn require_prefix_rejects_wrong_prefix() {
        let rel = PathBuf::from("assets/x.md");
        let err = require_prefix(&rel, "references").unwrap_err().to_string();
        assert!(err.contains("expected under references"));
    }

    #[test]
    fn require_prefix_accepts_correct() {
        let rel = PathBuf::from("references/x.md");
        assert!(require_prefix(&rel, "references").is_ok());
    }
}
