#!/usr/bin/env bash
# Shared k3d lifecycle for every repository cluster E2E.
#
# Scenario scripts own topology, assertions, and fault timing. This harness owns
# only the repeated infrastructure mechanics: tool admission, cluster lifecycle,
# host-built executable resolution, single-platform image import, and CoreDNS
# refresh. Keeping those mechanics here prevents subtly different offline-image
# and kubelet policies from becoming parallel test environments.

set -euo pipefail

K3D_PLATFORM="${AWAKEN_K3D_PLATFORM:-linux/amd64}"

k3d_validate_name() {
  [[ "$1" =~ ^[a-z0-9][a-z0-9-]*$ ]]
}

k3d_require_tools() {
  command -v k3d >/dev/null 2>&1
  command -v kubectl >/dev/null 2>&1
  command -v docker >/dev/null 2>&1
  docker info >/dev/null 2>&1
}

# k3d_docker_build <docker build arguments...>
#
# Repository k3d scenarios build images on the host before importing them into
# the cluster. Some CI and developer Docker bridges do not have working DNS,
# even though the host does. Keep that environment concern out of individual
# scenarios while preserving Docker's default network unless the caller
# explicitly selects another one (for example, `host`).
k3d_docker_build() {
  if [[ -n "${AWAKEN_K3D_BUILD_NETWORK:-}" ]]; then
    docker build --network "$AWAKEN_K3D_BUILD_NETWORK" "$@"
  else
    docker build "$@"
  fi
}

# k3d_admit_or_exit <scenario-label> [required: 0|1]
#
# This is the single process-level admission policy for repository k3d tests.
# Release callers select strict behavior with AWAKEN_K3D_REQUIRED=1 (or the
# explicit second argument); developer invocations remain an intentional skip
# when their machine has no cluster tooling.
k3d_admit_or_exit() {
  local scenario="$1"
  local required="${2:-${AWAKEN_K3D_REQUIRED:-0}}"
  [[ "$required" = "0" || "$required" = "1" ]] || {
    echo "invalid k3d required flag: $required" >&2
    exit 2
  }
  k3d_require_tools && return 0
  if [[ "$required" = "1" ]]; then
    echo "$scenario requires k3d, kubectl, and a reachable Docker daemon" >&2
    exit 1
  fi
  echo "k3d/kubectl/docker unavailable; skipping $scenario"
  exit 0
}

k3d_delete_cluster() {
  local cluster="$1"
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  k3d cluster delete "$cluster" >/dev/null 2>&1 || true
}

# k3d_create_cluster <name> <agent-count> [eviction-percent] [registry-coordinate]
k3d_create_cluster() {
  local cluster="$1" agents="$2" eviction="${3:-2}" registry="${4:-}"
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  [[ "$agents" =~ ^[0-9]+$ ]] || {
    echo "invalid k3d agent count: $agents" >&2
    return 2
  }
  [[ "$eviction" =~ ^[0-9]+$ ]] || {
    echo "invalid k3d eviction percentage: $eviction" >&2
    return 2
  }
  [[ -z "$registry" || "$registry" =~ ^[a-zA-Z0-9.-]+:[0-9]+$ ]] || {
    echo "invalid k3d Registry coordinate: $registry" >&2
    return 2
  }
  k3d_delete_cluster "$cluster"
  local threshold="eviction-hard=imagefs.available<${eviction}%,nodefs.available<${eviction}%"
  local args=(
    cluster create "$cluster" --agents "$agents" --wait --timeout 180s
    --runtime-ulimit "nofile=65536:65536"
    --k3s-arg "--kubelet-arg=$threshold@server:*"
  )
  if (( agents > 0 )); then
    args+=(--k3s-arg "--kubelet-arg=$threshold@agent:*")
  fi
  [[ -z "$registry" ]] || args+=(--registry-use "$registry")
  k3d "${args[@]}" >/dev/null
}

k3d_archive_name() {
  local value="${1//[^a-zA-Z0-9_.-]/_}"
  printf '%s' "$value"
}

# Return the first IPv4 loopback port available at or above the preferred port.
# This avoids false E2E failures when a previous kubectl tunnel left the same
# static port in TIME_WAIT.
k3d_available_port() {
  local preferred="$1"
  [[ "$preferred" =~ ^[0-9]+$ ]] && (( preferred >= 1024 && preferred <= 65535 )) || return 2
  python3 - "$preferred" <<'PY'
import socket
import sys

preferred = int(sys.argv[1])
for port in range(preferred, min(preferred + 100, 65536)):
    candidate = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        candidate.bind(("127.0.0.1", port))
    except OSError:
        candidate.close()
        continue
    candidate.close()
    print(port)
    raise SystemExit(0)
raise SystemExit("no available IPv4 loopback port in the candidate range")
PY
}

# k3d_start_port_forward <namespace> <svc/name|pod/name> <local-port> <remote-port> <log-file>
# Force IPv4 because every scenario probes 127.0.0.1; kubectl's localhost
# resolution can otherwise leave only an IPv6 listener on some developer hosts.
k3d_start_port_forward() {
  local namespace="$1" resource="$2" local_port="$3" remote_port="$4" log_file="$5"
  [[ "$namespace" =~ ^[a-z0-9][a-z0-9-]*$ ]] || return 2
  [[ "$resource" =~ ^(svc|pod)/[a-z0-9][a-z0-9.-]*$ ]] || return 2
  [[ "$local_port" =~ ^[0-9]+$ && "$remote_port" =~ ^[0-9]+$ ]] || return 2
  kubectl -n "$namespace" port-forward --address=127.0.0.1 \
    "$resource" "$local_port:$remote_port" >"$log_file" 2>&1 &
  printf '%s' "$!"
}

# k3d_import_images <cluster> <application/dependency image>...
# Pause and CoreDNS are always included because every offline topology needs
# both before any scenario Pod can become Ready.
k3d_import_images() {
  local cluster="$1"
  shift
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  local node="k3d-$cluster-server-0"
  local pause_image
  pause_image=$(docker exec "$node" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null || true)
  pause_image=${pause_image:-rancher/mirrored-pause:3.6}
  local coredns_image
  coredns_image=$(kubectl -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
  coredns_image=${coredns_image:-rancher/mirrored-coredns-coredns:1.10.1}

  local images=("$pause_image" "$coredns_image" "$@")
  local unique=() image existing
  for image in "${images[@]}"; do
    existing=0
    local candidate
    for candidate in "${unique[@]}"; do
      [[ "$candidate" = "$image" ]] && existing=1 && break
    done
    (( existing == 0 )) && unique+=("$image")
  done

  local archive_dir
  archive_dir=$(mktemp -d)
  local archives=() status=0
  for image in "${unique[@]}"; do
    docker image inspect "$image" >/dev/null 2>&1 \
      || docker pull -q "$image" >/dev/null \
      || { status=$?; break; }
    local archive="$archive_dir/$(k3d_archive_name "$image").tar"
    docker save --platform "$K3D_PLATFORM" -o "$archive" "$image" \
      || { status=$?; break; }
    archives+=("$archive")
  done
  if (( status == 0 )); then
    k3d image import "${archives[@]}" -c "$cluster" >/dev/null || status=$?
  fi
  rm -rf "$archive_dir"
  # k3d copies every imported archive into its shared /k3d/images transport
  # volume. The node has already consumed those archives when `image import`
  # returns, so retaining them only creates a second, unbounded image store.
  # Clean the transport files on both success and failure; containerd remains
  # the authoritative runtime image store.
  docker exec "$node" find /k3d/images -mindepth 1 -maxdepth 1 -type f -delete \
    >/dev/null 2>&1 || true
  (( status == 0 )) || return "$status"
  kubectl -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
  kubectl -n kube-system rollout status deploy/coredns --timeout=90s
}

# resolve_cargo_executable <package> <binary> [cargo arguments...]
resolve_cargo_executable() {
  local package="$1" binary="$2"
  shift 2
  local cargo_target_dir="${CARGO_TARGET_DIR:-target}"
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.96.0}" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo build -q -p "$package" --bin "$binary" "$@"
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.96.0}" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo build -p "$package" --bin "$binary" "$@" --message-format=json 2>/dev/null \
    | BINARY_TARGET="$binary" python3 -c 'import json, os, sys
target = os.environ["BINARY_TARGET"]
for line in sys.stdin:
    try:
        message = json.loads(line)
    except Exception:
        continue
    if message.get("executable") and message.get("target", {}).get("name") == target:
        print(message["executable"])' \
    | tail -1
}

k3d_harness_selftest() (
  # Cause/effect decision table:
  # H1 canonical name/count/percentage -> one delete + create with server and agent
  # thresholds; H2 zero agents -> no agent threshold; H3 option-shaped name or
  # non-numeric count/percentage -> rejected before any k3d effect; H4 duplicate
  # image coordinates -> one single-platform archive per unique image plus the
  # canonical Pause/CoreDNS prerequisites, then delete k3d's redundant transport
  # copies; H5 malformed port-forward identity ->
  # rejection before kubectl; H6 valid preferred port -> an available IPv4 port;
  # H7 malformed preferred port -> rejection before a socket probe; H8 an exact
  # Registry coordinate is attached once and malformed input is rejected before
  # cluster mutation; H9 tools available -> continue in optional and strict mode;
  # H10 tools unavailable + optional mode -> successful explicit skip; H11 tools
  # unavailable + strict mode -> failure; H12 malformed strictness -> configuration
  # failure; H13 no build-network override -> Docker's default build network;
  # H14 explicit override -> exactly one typed Docker build network option.
  # The scenario tests own network and topology effects.
  local k3d_calls=() docker_saves=() docker_execs=() docker_builds=()
  k3d() { k3d_calls+=("$*"); }

  k3d_validate_name "awaken-test-1"
  ! k3d_validate_name "--all"
  k3d_create_cluster "awaken-test-1" 2 3
  [[ "${k3d_calls[0]}" = "cluster delete awaken-test-1" ]]
  [[ "${k3d_calls[1]}" = *"--kubelet-arg=eviction-hard=imagefs.available<3%,nodefs.available<3%@server:*"* ]]
  [[ "${k3d_calls[1]}" = *"--kubelet-arg=eviction-hard=imagefs.available<3%,nodefs.available<3%@agent:*"* ]]

  k3d_calls=()
  k3d_create_cluster "awaken-test-1" 0 2
  [[ "${k3d_calls[1]}" != *"@agent:*"* ]]

  k3d_calls=()
  ! k3d_create_cluster "--all" 1 2
  ! k3d_create_cluster "awaken-test" many 2
  ! k3d_create_cluster "awaken-test" 1 low
  (( ${#k3d_calls[@]} == 0 ))
  k3d_create_cluster "awaken-test" 1 2 "k3d-awaken-registry.localhost:5000"
  [[ "${k3d_calls[1]}" = *"--registry-use k3d-awaken-registry.localhost:5000"* ]]
  k3d_calls=()
  ! k3d_create_cluster "awaken-test" 1 2 "--all"
  (( ${#k3d_calls[@]} == 0 ))
  ! k3d_start_port_forward "--all" "svc/brain" 38080 3000 /tmp/unused
  ! k3d_start_port_forward "awaken-test" "deployment/brain" 38080 3000 /tmp/unused
  local available_port
  available_port=$(k3d_available_port 43000)
  [[ "$available_port" =~ ^[0-9]+$ ]] && (( available_port >= 43000 ))
  ! k3d_available_port 80
  ! k3d_available_port many

  # Admission cause/effect decision table:
  # | Rule | tools available | required | valid flag | Effect |
  # | H9   | T               | *        | T          | continue caller |
  # | H10  | F               | F        | T          | exit 0 with explicit skip |
  # | H11  | F               | T        | T          | exit 1; release gate fails |
  # | H12  | *               | *        | F          | exit 2; reject configuration |
  k3d_require_tools() { return 0; }
  k3d_admit_or_exit "fixture" 0
  k3d_admit_or_exit "fixture" 1
  k3d_require_tools() { return 1; }
  local admission_output admission_status
  set +e
  admission_output=$(k3d_admit_or_exit "fixture" 0 2>&1)
  admission_status=$?
  set -e
  [[ "$admission_status" = "0" ]]
  [[ "$admission_output" = *"skipping fixture"* ]]
  set +e
  admission_output=$(k3d_admit_or_exit "fixture" 1 2>&1)
  admission_status=$?
  set -e
  [[ "$admission_status" = "1" ]]
  [[ "$admission_output" = *"fixture requires k3d"* ]]
  set +e
  admission_output=$(k3d_admit_or_exit "fixture" invalid 2>&1)
  admission_status=$?
  set -e
  [[ "$admission_status" = "2" ]]
  [[ "$admission_output" = *"invalid k3d required flag"* ]]

  [[ "$(k3d_archive_name 'registry:5000/awaken@sha256:abc')" = "registry_5000_awaken_sha256_abc" ]]
  docker() {
    if [[ "$1" = exec ]]; then
      docker_execs+=("$*")
      if [[ "$*" = *"sandbox_image"* ]]; then
        printf '%s' 'rancher/mirrored-pause:3.6'
      fi
    elif [[ "$1 $2" = "image inspect" ]]; then
      return 0
    elif [[ "$1" = save ]]; then
      docker_saves+=("$*")
    elif [[ "$1 $2" = "build --network" || "$1" = build ]]; then
      docker_builds+=("$*")
    fi
  }
  kubectl() {
    if [[ "$*" = *"get deploy coredns"* ]]; then
      printf '%s' 'rancher/mirrored-coredns-coredns:1.10.1'
    fi
  }
  k3d_calls=()
  k3d_import_images "awaken-test" app:latest app:latest postgres:16
  (( ${#docker_saves[@]} == 4 ))
  [[ "${docker_saves[*]}" = *"--platform $K3D_PLATFORM"* ]]
  [[ "${k3d_calls[0]}" = image\ import* ]]
  [[ "${docker_execs[-1]}" = "exec k3d-awaken-test-server-0 find /k3d/images -mindepth 1 -maxdepth 1 -type f -delete" ]]

  unset AWAKEN_K3D_BUILD_NETWORK
  k3d_docker_build --load -t fixture:latest .
  [[ "${docker_builds[-1]}" = "build --load -t fixture:latest ." ]]
  AWAKEN_K3D_BUILD_NETWORK=host k3d_docker_build --load -t fixture:latest .
  [[ "${docker_builds[-1]}" = "build --network host --load -t fixture:latest ." ]]
)

if [[ "${BASH_SOURCE[0]}" = "$0" ]]; then
  if [[ "${1:-}" = "--self-test" ]]; then
    k3d_harness_selftest
    echo "OK - k3d harness admission rules hold."
  else
    echo "usage: $0 --self-test" >&2
    exit 2
  fi
fi
