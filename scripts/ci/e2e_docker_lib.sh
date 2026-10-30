#!/usr/bin/env bash
# Docker lifecycle helpers shared by the e2e and CI harness scripts.
#
# Images such as postgres, minio, registry, k3s and vault declare VOLUME, so a
# container started without an explicit mount gets an anonymous volume that
# `docker rm` keeps and that no later command can attribute to a run. The
# helpers here make every harness object attributable and reclaimable:
#
#   e2e_docker_init <owner>        -> E2E_LABELS (--label ... pairs) and E2E_RUN_ID
#   e2e_docker_sweep [max_hours]   -> drop the previous runs' leftovers of <owner>
#   e2e_k3d_node_volumes <cluster> -> volumes mounted by the cluster's containers
#   e2e_docker_rm_volumes <name>.. -> best-effort `docker volume rm`
#
# The sweep is the only part that survives SIGKILL: the next run removes what a
# killed run could not. It never touches an object without this owner's label,
# and never a container that is running and younger than the age bound.
#
# Source this file; it sets no shell options.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo "usage: source scripts/ci/e2e_docker_lib.sh (sourced by harness scripts, not executed)" >&2
  exit 2
fi

E2E_OWNER_LABEL="awaken.e2e.owner"
E2E_RUN_LABEL="awaken.e2e.run"

e2e_docker_init() {
  E2E_OWNER="$1"
  E2E_RUN_ID="${AWAKEN_E2E_RUN_ID:-$(date +%s)-$$}"
  E2E_LABELS=(--label "${E2E_OWNER_LABEL}=${E2E_OWNER}" --label "${E2E_RUN_LABEL}=${E2E_RUN_ID}")
}

e2e_docker_rm_volumes() {
  local name
  for name in "$@"; do
    [[ -z "$name" ]] || docker volume rm -f "$name" >/dev/null 2>&1 || true
  done
}

# True when a container of another run may be removed: it is not running, or it
# has run longer than the age bound.
_e2e_container_is_stale() {
  local id="$1" max_hours="$2" state started started_epoch now
  state="$(docker inspect --format '{{.State.Status}}' "$id" 2>/dev/null || true)"
  [[ -n "$state" ]] || return 1
  [[ "$state" == running ]] || return 0
  now="$(date +%s)"
  started="$(docker inspect --format '{{.State.StartedAt}}' "$id" 2>/dev/null || true)"
  started_epoch="$(date -d "$started" +%s 2>/dev/null || echo "$now")"
  (( now - started_epoch > max_hours * 3600 ))
}

# Sweep decision table (container state x age x run label):
#   | state            | run label        | action                              |
#   | not running      | any other run    | remove with its volumes             |
#   | running, older   | any other run    | remove with its volumes             |
#   | running, younger | any other run    | keep: may be a live run             |
#   | any              | current run      | keep                                |
# A labelled container of a k3d cluster is removed by deleting its cluster. A
# labelled volume of another run goes with the containers that still mount it
# (a k3d registry cannot be labelled, so its labelled volume names it).
e2e_docker_sweep() {
  local max_hours="${1:-6}" id run name cluster
  local -a clusters=() held=()
  while IFS= read -r id; do
    [[ -n "$id" ]] || continue
    run="$(docker inspect --format "{{index .Config.Labels \"${E2E_RUN_LABEL}\"}}" "$id" 2>/dev/null || true)"
    [[ "$run" != "$E2E_RUN_ID" ]] || continue
    _e2e_container_is_stale "$id" "$max_hours" || continue
    cluster="$(docker inspect --format '{{index .Config.Labels "k3d.cluster"}}' "$id" 2>/dev/null || true)"
    if [[ -n "$cluster" ]] && command -v k3d >/dev/null; then
      clusters+=("$cluster")
    else
      docker rm -f -v "$id" >/dev/null 2>&1 || true
    fi
  done < <(docker ps -aq --filter "label=${E2E_OWNER_LABEL}=${E2E_OWNER}" 2>/dev/null || true)
  for cluster in $(printf '%s\n' "${clusters[@]}" | sort -u); do
    held=($(e2e_k3d_node_volumes "$cluster"))
    k3d cluster delete "$cluster" >/dev/null 2>&1 || true
    e2e_docker_rm_volumes "${held[@]}"
  done
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    run="$(docker volume inspect --format "{{index .Labels \"${E2E_RUN_LABEL}\"}}" "$name" 2>/dev/null || true)"
    [[ "$run" != "$E2E_RUN_ID" ]] || continue
    while IFS= read -r id; do
      [[ -n "$id" ]] || continue
      _e2e_container_is_stale "$id" "$max_hours" && docker rm -f -v "$id" >/dev/null 2>&1 || true
    done < <(docker ps -aq --filter "volume=${name}" 2>/dev/null || true)
    docker volume rm "$name" >/dev/null 2>&1 || true
  done < <(docker volume ls -q --filter "label=${E2E_OWNER_LABEL}=${E2E_OWNER}" 2>/dev/null || true)
}

# Names of every volume mounted by the containers of a k3d cluster. Capture them
# before `k3d cluster delete`, then pass them to e2e_docker_rm_volumes after it:
# whatever the k3d release already removed is simply absent.
e2e_k3d_node_volumes() {
  local id
  while IFS= read -r id; do
    [[ -n "$id" ]] || continue
    docker inspect --format '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}}{{"\n"}}{{end}}{{end}}' "$id" 2>/dev/null || true
  done < <(docker ps -aq --filter "label=k3d.cluster=$1" 2>/dev/null || true) | sort -u
}

# Names of the volumes mounted by one container (k3d names its registry k3d-<name>).
e2e_container_volumes() {
  docker inspect --format '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}}{{"\n"}}{{end}}{{end}}' "$1" 2>/dev/null || true
}
