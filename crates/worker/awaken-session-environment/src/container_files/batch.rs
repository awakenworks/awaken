// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: the bounded command planner inside the existing Container read-only tree
// @formal reason: installer; it adds no credential materialization, archive extractor, catalog or
// @formal reason: lifecycle authority. Dynamic path: validate every relative path and collision before
// @formal reason: effects, transfer binary files into one private stage through bounded detached commands,
// @formal reason: restrict permissions, then use a separate authority-checked spawn for atomic
// @formal reason: publication; failures preserve the prior tree and cleanup rejects symlink ancestors.
// @formal reason: Adjacent cause-effect tests execute the actual portable program, and real Kubernetes
// @formal reason: consumers verify fsGroup and retained data. This is a product-requirement
// @formal reason: classification, not a direct formal proof of shell or filesystem behavior.
//! Bounded detached transfer for the one atomic read-only tree installer.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

const COMMAND_BYTES: usize = 64 * 1024;
const SCRIPT: &str = r#"set -efu
workspace=$1; stage=$2; root=$3; backup=$4; subdir=$5; first=$6; last=$7; shift 7
current=$workspace; old_ifs=$IFS; IFS=/
for part in $subdir; do current="$current/$part"; test ! -L "$current" || exit 65; done
IFS=$old_ifs
if [ "$first" = cleanup ]; then
  chmod -R u+w -- "$stage" 2>/dev/null || true; rm -rf -- "$stage"
  exit 0
fi
if [ "$first" = publish ]; then
  test -d "$stage" && test ! -L "$stage" || exit 65
  test ! -L "$backup" || exit 65
  chmod -R u+w -- "$backup" 2>/dev/null || true; rm -rf -- "$backup"
  if test -e "$root"; then mv -- "$root" "$backup" || exit 66; fi
  if mv -- "$stage" "$root"; then
    chmod -R u+w -- "$backup" 2>/dev/null || true; rm -rf -- "$backup"
  else
    test ! -e "$backup" || mv -- "$backup" "$root"
    exit 67
  fi
  exit 0
fi
if [ "$first" = 1 ]; then
  test ! -e "$stage" && test ! -L "$stage" || exit 65
  umask 077; mkdir -p -- "$stage"
fi
test -d "$stage" && test ! -L "$stage" || exit 65
while [ "$#" -gt 0 ]; do
  operation=$1; relative=$2; data=$3; executable=$4; shift 4
  path="$stage/$relative"; test ! -L "$path" || exit 65
  if [ "$operation" = new ]; then
    umask 077; mkdir -p -- "$(dirname -- "$path")"
    (set -C; printf %s "$data" | base64 -d > "$path")
  else
    test -f "$path"; printf %s "$data" | base64 -d >> "$path"
  fi
  if [ "$executable" = 1 ]; then chmod u+x -- "$path"; fi
done
if [ "$last" = 1 ]; then
  chmod -R a-w -- "$stage"
fi
"#;

fn arguments(subdir: &str, stage: &str, root: &str, backup: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        SCRIPT.into(),
        "awaken-read-only-tree".into(),
        pc::WorkspaceLayout::ROOT.into(),
        stage.into(),
        root.into(),
        backup.into(),
        subdir.into(),
        "0".into(),
        "0".into(),
    ]
}

pub(super) fn cleanup_command(stage: &str) -> pc::Command {
    let relative = stage
        .strip_prefix("/workspace/")
        .expect("tree stage remains within workspace");
    let mut argv = arguments(relative, stage, stage, stage);
    argv[9] = "cleanup".into();
    pc::Command {
        argv,
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    }
}

pub(super) fn commands(
    subdir: &str,
    files: &[(String, Vec<u8>, bool)],
    sequence: u64,
) -> Result<(String, Vec<pc::Command>), pc::SandboxError> {
    use base64::Engine as _;
    let root = workspace_path(subdir)?;
    if root == pc::WorkspaceLayout::ROOT {
        return Err(pc::SandboxError::invalid(
            "read-only tree cannot replace workspace root",
        ));
    }
    let stage = format!("{root}.awaken-tree-stage-{}-{sequence}", std::process::id());
    let backup = format!(
        "{root}.awaken-tree-backup-{}-{sequence}",
        std::process::id()
    );
    // Validate the complete input before any remote mutation. No new archive
    // format/extractor or file staging inside the atomic tree stage is needed.
    let mut paths = std::collections::BTreeSet::new();
    let mut canonical = Vec::with_capacity(files.len());
    for (relative, _, _) in files {
        let path = read_only_tree_file_path(&stage, relative)?;
        let relative = path
            .strip_prefix(&format!("{stage}/"))
            .expect("validated stage path")
            .to_owned();
        if !paths.insert(relative.clone()) {
            return Err(pc::SandboxError::invalid("duplicate read-only tree path"));
        }
        canonical.push(relative);
    }
    for path in &paths {
        let mut parent = path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if paths.contains(prefix) {
                return Err(pc::SandboxError::invalid(
                    "read-only tree file/directory collision",
                ));
            }
            parent = prefix;
        }
    }
    let base = arguments(subdir, &stage, &root, &backup);
    let base_bytes: usize = base.iter().map(|value: &String| value.len() + 1).sum();
    let mut batches = vec![base.clone()];
    batches[0][9] = "1".into();
    let mut used = base_bytes;
    for ((_, contents, executable), relative) in files.iter().zip(canonical) {
        let pieces = contents.len().div_ceil(MATERIALIZATION_CHUNK_BYTES).max(1);
        for index in 0..pieces {
            let start = index * MATERIALIZATION_CHUNK_BYTES;
            let end = (start + MATERIALIZATION_CHUNK_BYTES).min(contents.len());
            let operation = vec![
                if index == 0 {
                    "new".into()
                } else {
                    "append".into()
                },
                relative.clone(),
                base64::engine::general_purpose::STANDARD.encode(&contents[start..end]),
                if *executable && index + 1 == pieces {
                    "1".into()
                } else {
                    "0".into()
                },
            ];
            let bytes: usize = operation.iter().map(|value: &String| value.len() + 1).sum();
            if base_bytes + bytes > COMMAND_BYTES {
                return Err(pc::SandboxError::invalid(
                    "read-only tree command exceeds transfer bound",
                ));
            }
            if used + bytes > COMMAND_BYTES {
                batches.push(base.clone());
                used = base_bytes;
            }
            batches.last_mut().expect("batch exists").extend(operation);
            used += bytes;
        }
    }
    if base_bytes > COMMAND_BYTES {
        return Err(pc::SandboxError::invalid(
            "read-only tree command exceeds transfer bound",
        ));
    }
    batches.last_mut().expect("batch exists")[10] = "1".into();
    // Publication is a distinct spawn so the existing Environment/lease fence
    // is checked again after transfer, before replacing the visible root.
    let mut publish = base;
    publish[9] = "publish".into();
    batches.push(publish);
    Ok((
        stage,
        batches
            .into_iter()
            .map(|argv| pc::Command {
                argv,
                cwd: pc::WorkspaceLayout::ROOT.into(),
                env: Vec::new(),
                stdio: pc::Stdio::Null,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests;
