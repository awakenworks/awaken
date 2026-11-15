#!/usr/bin/env bash
# Run by the protected tag workflow after building the Linux role binary.
set -euo pipefail

usage() { echo 'usage: bash scripts/release/publish_runtime_image.sh {coordinator|worker}' >&2; }
if [[ $# != 1 || ! "$1" =~ ^(coordinator|worker)$ ]]; then
  usage
  exit 2
fi
role=$1
binary="awaken-$role"
repository="ghcr.io/$GITHUB_REPOSITORY_OWNER/$binary"
platform=linux/amd64
test "$GITHUB_EVENT_NAME" = push
test "$GITHUB_REPOSITORY" = awakenworks/awaken
test "$GITHUB_REPOSITORY_OWNER" = awakenworks
test "$GITHUB_REF_TYPE" = tag
test "$GITHUB_REF_PROTECTED" = true
test "$GITHUB_REF" = "refs/tags/$GITHUB_REF_NAME"
[[ "$GITHUB_REF_NAME" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
revision=$(git rev-parse "$GITHUB_SHA^{commit}")
test "$(git rev-parse HEAD)" = "$revision"
[[ "$revision" =~ ^[0-9a-f]{40}$ ]]
tag="$repository:$GITHUB_REF_NAME"
staging="$repository:staging-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT"
identity="https://github.com/$GITHUB_REPOSITORY/$WORKFLOW_PATH@$GITHUB_REF"
source_binary="target/release/$binary"
scratch=$(mktemp -d "$RUNNER_TEMP/runtime-image.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

# Only an explicit missing manifest allows publication. Permission, network,
# and unexpected output errors must leave the release tag alone.
resolve_tag() {
  local output digest
  if output=$(docker buildx imagetools inspect "$tag" 2>"$scratch/inspect.stderr"); then
    digest=$(sed -n 's/^Digest:[[:space:]]*//p' <<<"$output" | head -n 1)
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || return 2
    printf '%s\n' "$digest"
  elif { grep -Eqi 'manifest unknown|name unknown|no such manifest' "$scratch/inspect.stderr" \
         || grep -Fqx "ERROR: $tag: not found" "$scratch/inspect.stderr"; } \
      && ! grep -Eqi 'unauthorized|denied|forbidden|authentication|insufficient.scope|(^|[^0-9])(401|403)([^0-9]|$)' "$scratch/inspect.stderr"; then
    return 1
  else
    cat "$scratch/inspect.stderr" >&2
    return 2
  fi
}

verify_signature() {
  cosign verify --certificate-identity "$identity" \
    --certificate-oidc-issuer "$SIGSTORE_OIDC_ISSUER" \
    --certificate-github-workflow-repository "$GITHUB_REPOSITORY" \
    --certificate-github-workflow-ref "$GITHUB_REF" \
    --certificate-github-workflow-sha "$revision" \
    --certificate-github-workflow-trigger push "$1" >/dev/null
}

new_image=0
if digest=$(resolve_tag); then
  verify_signature "$repository@$digest"
else
  status=$?
  test "$status" = 1 || exit "$status"
  mkdir "$scratch/context"
  install -m 0755 "$source_binary" "$scratch/context/$binary"
  docker build --platform "$platform" --file "deploy/images/$role/Dockerfile" \
    --label "org.opencontainers.image.source=$SOURCE_REPOSITORY" \
    --label "org.opencontainers.image.revision=$revision" \
    --label "org.opencontainers.image.version=$GITHUB_REF_NAME" \
    --tag "$staging" "$scratch/context"
  docker push "$staging" | tee "$scratch/push.log"
  digest=$(sed -n 's/^.*digest: \(sha256:[0-9a-f]\{64\}\).*$/\1/p' "$scratch/push.log" | tail -n 1)
  [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
  new_image=1
fi

image="$repository@$digest"
docker pull --platform "$platform" "$image"
test "$(docker image inspect --format '{{.Os}}/{{.Architecture}} {{.Config.User}}' "$image")" = "$platform 10001"
test "$(docker image inspect --format '{{index .Config.Entrypoint 0}}' "$image")" = "/usr/local/bin/$binary"
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.source"}}' "$image")" = "$SOURCE_REPOSITORY"
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image")" = "$revision"
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$image")" = "$GITHUB_REF_NAME"
run=(docker run --rm --network none --read-only --cap-drop ALL)
"${run[@]}" "$image" --help >/dev/null
if [[ "$role" = coordinator ]]; then
  "${run[@]}" "$image" database migrate --help >/dev/null
else
  "${run[@]}" --entrypoint python3 "$image" --version >/dev/null
fi

if [[ "$new_image" = 1 ]]; then
  expected=$(sha256sum "$source_binary" | cut -d ' ' -f 1)
  actual=$("${run[@]}" --entrypoint sha256sum "$image" "/usr/local/bin/$binary" | cut -d ' ' -f 1)
  test "$actual" = "$expected"
  cosign sign --yes "$image"
  verify_signature "$image"
  if before=$(resolve_tag); then
    test "$before" = "$digest"
  else
    status=$?
    test "$status" = 1 || exit "$status"
    docker buildx imagetools create --prefer-index=false --tag "$tag" "$image"
  fi
fi
test "$(resolve_tag)" = "$digest"

printf 'repository=%s\ndigest=%s\n' "$repository" "$digest" >>"$GITHUB_OUTPUT"
printf '{"role":"%s","platform":"%s","image":"%s","revision":"%s","version":"%s"}\n' \
  "$role" "$platform" "$image" "$revision" "$GITHUB_REF_NAME" >"$RUNNER_TEMP/$role-image.json"
