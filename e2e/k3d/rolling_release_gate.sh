#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash e2e/k3d/rolling_release_gate.sh
Set ADR71_ROLLING_REGISTRY to the trusted internal registry host (and optional port).
Set all four role images to repository@sha256:<digest> in that registry:
  ADR71_ROLLING_LEGACY_IMAGE         ADR71_ROLLING_CANDIDATE_IMAGE
  ADR71_ROLLING_LEGACY_WORKER_IMAGE  ADR71_ROLLING_CANDIDATE_WORKER_IMAGE
Set ADR71_ROLLING_EVIDENCE_DIR to the parent directory for per-run evidence.
Set ADR71_KEEP_FAILED_CLUSTER=1 to keep the failed cluster for investigation.
USAGE
}

if [[ "$#" = 1 && "$1" = --help ]]; then
  usage
  exit 0
fi
if [[ "$#" != 0 || -z "${ADR71_ROLLING_EVIDENCE_DIR:-}" ]]; then
  usage >&2
  exit 2
fi

mkdir -p -- "$ADR71_ROLLING_EVIDENCE_DIR"
run=$(mktemp -d -- "$ADR71_ROLLING_EVIDENCE_DIR/rolling-release-XXXXXXXX")
run=$(cd "$run" && pwd)
export ADR71_ROLLING_EVIDENCE_DIR="$run/rollout"
run_name=${run##*/}
export AWAKEN_K3D_CLUSTER="${AWAKEN_K3D_CLUSTER:-${run_name,,}}"
export ADR71_ROLLING_REQUIRE_RELEASE=1
printf 'Release gate evidence: %s\n' "$run"

run_gate() {
  local name
  for name in ADR71_ROLLING_REGISTRY ADR71_ROLLING_LEGACY_IMAGE ADR71_ROLLING_CANDIDATE_IMAGE \
      ADR71_ROLLING_LEGACY_WORKER_IMAGE ADR71_ROLLING_CANDIDATE_WORKER_IMAGE; do
    if [[ -z "${!name:-}" ]]; then
      printf 'Missing %s\n' "$name" >&2
      return 2
    fi
    printf '%s=%s\n' "$name" "${!name}"
  done
  case "${ADR71_ROLLING_REGISTRY%%:*}" in
    ghcr.io|docker.io|index.docker.io|registry-1.docker.io)
      printf 'Configure the internal image registry for release verification\n' >&2
      return 2 ;;
  esac
  bash "$(dirname "$0")/distributed_control_e2e.sh"
}

status=0
run_gate 2>&1 | tee "$run/run.log" || status=$?
printf '%s\n' "$status" >"$run/exit-code"
exit "$status"
