#!/usr/bin/env bash
# Docker lifecycle helpers shared by the e2e and CI harness scripts.
# Labels identify this invocation; neither a functional owner nor resource age
# authorizes cleanup of another run. Scenario traps own their creation receipts.
# The cluster lifecycle retains actual node/network IDs and volume creation
# facts. This creator selects shared data and the empty volume required by the
# pinned K3D startup; a per-request label distinguishes a new object from reuse.
# Source this file; it sets no shell options.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo "usage: source scripts/ci/e2e_docker_lib.sh (sourced by harness scripts, not executed)" >&2
  exit 2
fi

E2E_OWNER_LABEL="awaken.e2e.owner"
E2E_RUN_LABEL="awaken.e2e.run"
E2E_CREATION_LABEL="awaken.e2e.creation"
E2E_OWNER=""
E2E_RUN_ID=""
E2E_LABELS=()
E2E_VOLUME_RECEIPTS=()
E2E_CREATED_VOLUME=""
E2E_UNCONFIRMED_VOLUME=""

e2e_docker_init() {
  E2E_OWNER="$1"
  E2E_RUN_ID="${AWAKEN_E2E_RUN_ID:-$(date +%s)-$$}"
  [[ "$E2E_OWNER" =~ ^[a-zA-Z0-9_.-]+$ && "$E2E_RUN_ID" =~ ^[a-zA-Z0-9_.-]+$ ]] || return 2
  E2E_LABELS=(--label "${E2E_OWNER_LABEL}=${E2E_OWNER}" --label "${E2E_RUN_LABEL}=${E2E_RUN_ID}")
}

# Invoke directly: the creation receipt stays in this shell. Ordinary data
# volumes get fresh Docker-generated names. Only K3D's required transport-volume
# interop supplies --name; a unique request label detects Docker's silent reuse
# of an already-existing name, even when its owner/run labels happen to match.
e2e_docker_create_volume() {
  local requested="" label name observed actual_name created owner run nonce actual_nonce extra create_status=0
  [[ -z "$E2E_UNCONFIRMED_VOLUME" ]] || return 2
  if [[ "${1:-}" = --name ]]; then
    [[ "$#" -ge 2 && "$2" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]] || return 2
    requested="$2"; shift 2
  fi
  local args=("${E2E_LABELS[@]}")
  E2E_CREATED_VOLUME=""
  nonce=$(python3 -c 'import uuid; print(uuid.uuid4().hex)') || return "$?"
  [[ "$nonce" =~ ^[0-9a-f]{32}$ ]] || return 2
  args+=(--label "$E2E_CREATION_LABEL=$nonce")
  for label in "$@"; do args+=(--label "$label"); done
  [[ -z "$requested" ]] || args+=("$requested")
  E2E_UNCONFIRMED_VOLUME="${requested:-<generated-name-unknown>}"
  name=$(docker volume create "${args[@]}") || create_status=$?
  [[ -z "$name" ]] || E2E_UNCONFIRMED_VOLUME="$name"
  [[ "$create_status" = 0 ]] || return "$create_status"
  if [[ -n "$requested" ]]; then [[ "$name" = "$requested" ]] || return 2
  else [[ "$name" =~ ^[0-9a-f]{64}$ ]] || return 2; fi
  observed=$(docker volume inspect --format \
    "{{.Name}}|{{.CreatedAt}}|{{with index .Labels \"$E2E_OWNER_LABEL\"}}{{.}}{{end}}|{{with index .Labels \"$E2E_RUN_LABEL\"}}{{.}}{{end}}|{{with index .Labels \"$E2E_CREATION_LABEL\"}}{{.}}{{end}}" \
    "$name") || return "$?"
  [[ "$observed" != *$'\n'* ]] || return 2
  IFS='|' read -r actual_name created owner run actual_nonce extra <<<"$observed"
  [[ "$actual_name" = "$name" && -n "$created" && -z "$extra" ]] || return 2
  if [[ "$actual_nonce" != "$nonce" ]]; then
    # A successful Docker create reused an existing object unchanged. It is
    # definitively not this request's creation; never claim or remove it.
    E2E_UNCONFIRMED_VOLUME=""
    return 2
  fi
  [[ "$owner" = "$E2E_OWNER" && "$run" = "$E2E_RUN_ID" ]] || return 2
  local index replaced=0
  for index in "${!E2E_VOLUME_RECEIPTS[@]}"; do
    if [[ "${E2E_VOLUME_RECEIPTS[$index]%%|*}" = "$name" ]]; then
      E2E_VOLUME_RECEIPTS[$index]="$observed"
      replaced=1
    fi
  done
  [[ "$replaced" = 1 ]] || E2E_VOLUME_RECEIPTS+=("$observed")
  E2E_CREATED_VOLUME="$name"
  E2E_UNCONFIRMED_VOLUME=""
}

# Both explicit volume-create and retained anonymous node-volume creation facts
# use this ONE identity check. Failed reads are never confirmed absence.
_e2e_docker_check_created_volume() {
  local expected="$1" name="${1%%|*}" present observed
  present=$(docker volume ls --filter "name=^${name}$" --format '{{.Name}}') || return "$?"
  [[ -n "$present" ]] || return 0
  [[ "$present" = "$name" ]] || return 2
  observed=$(docker volume inspect --format \
    "{{.Name}}|{{.CreatedAt}}|{{with index .Labels \"$E2E_OWNER_LABEL\"}}{{.}}{{end}}|{{with index .Labels \"$E2E_RUN_LABEL\"}}{{.}}{{end}}|{{with index .Labels \"$E2E_CREATION_LABEL\"}}{{.}}{{end}}" \
    "$name") || return "$?"
  [[ "$observed" = "$expected" ]] || return 2
  printf '%s\n' "$observed"
}

_e2e_docker_owned_volume() {
  local name="$1" receipt
  for receipt in "${E2E_VOLUME_RECEIPTS[@]}"; do
    if [[ "${receipt%%|*}" = "$name" ]]; then
      _e2e_docker_check_created_volume "$receipt"
      return "$?"
    fi
  done
  return 2
}

# One checked borrower selection for K3D, detached runtimes and explicit volume
# teardown. No caller removes an unknown mounting container to free a volume.
_e2e_docker_check_volume_mounts() {
  local volume="$1" borrowers borrower retained found
  shift
  borrowers=$(docker ps -aq --no-trunc --filter "volume=$volume") || return "$?"
  for borrower in $borrowers; do
    found=0
    for retained in "$@"; do [[ "$borrower" != "$retained" ]] || found=1; done
    [[ "$found" = 1 ]] || return 2
  done
}

# This effect receives an identity just checked against its creator's retained
# receipt. It never selects another object or removes borrowers. Docker's
# non-force removal protects an endpoint that joins after our observation.
_e2e_docker_remove_created_volume() {
  local observed="$1" name="${1%%|*}" present
  [[ -n "$observed" ]] || return 0
  _e2e_docker_check_volume_mounts "$name" || return "$?"
  docker volume rm "$name" >/dev/null || return "$?"
  present=$(docker volume ls --filter "name=^${name}$" --format '{{.Name}}') || return "$?"
  [[ -z "$present" ]] || return 1
}

e2e_docker_rm_volumes() {
  local name observed
  for name in "$@"; do
    [[ -n "$name" ]] || continue
    observed=$(_e2e_docker_owned_volume "$name") || return "$?"
    _e2e_docker_remove_created_volume "$observed" || return "$?"
  done
}
