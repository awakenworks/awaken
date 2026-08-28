#!/usr/bin/env bash
# Resolve one Awaken Sandbox tag to immutable, identity-checked bytes.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <release-tag> <revision> <source-ref> <output-prefix>" >&2
  exit 2
fi

release_tag="$1"
revision="$2"
source_ref="$3"
output_prefix="$4"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
image_repository="ghcr.io/awakenworks/awaken-sandbox"
manifest="${output_prefix}.manifest.json"
labels="${output_prefix}.labels.json"
error="${output_prefix}.stderr"

[[ "$release_tag" == "${image_repository}:"* && "$release_tag" != *"@"* ]] || {
  echo "Awaken Sandbox release tag must belong to the canonical image repository" >&2
  exit 2
}
: > "$manifest"
: > "$labels"
: > "$error"

# Cause/effect decision table:
# tag absent -> report absent without a write;
# tag present + exact digest-bound OCI identity -> report that immutable image;
# ambiguous lookup, tag/config drift, or malformed metadata -> fail closed.
if docker buildx imagetools inspect "$release_tag" \
    --format '{{json .Manifest}}' > "$manifest" 2> "$error"; then
  immutable_image="$(python3 "$script_dir/awaken_sandbox_image_provenance.py" \
    resolve-manifest --manifest "$manifest")"
  docker buildx imagetools inspect "$immutable_image" \
    --format '{{json .Image.Config.Labels}}' > "$labels"
  python3 "$script_dir/awaken_sandbox_image_provenance.py" \
    validate-image-labels \
    --labels "$labels" \
    --revision "$revision" \
    --source-ref "$source_ref" >&2
  printf 'present %s\n' "$immutable_image"
  exit 0
fi

if [[ ! -s "$manifest" ]] && grep -Eqi \
    'manifest unknown|name unknown|no such manifest' "$error"; then
  echo absent
  exit 0
fi

cat "$error" >&2
exit 1
