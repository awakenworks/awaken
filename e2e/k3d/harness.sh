#!/usr/bin/env bash
# Shared k3d lifecycle for every repository cluster E2E.
#
# Scenario scripts own topology, assertions, and fault timing. This harness owns
# only the repeated infrastructure mechanics: tool admission, cluster lifecycle,
# host-built executable resolution, identity-preserving image import, and CoreDNS
# refresh. Keeping those mechanics here prevents subtly different offline-image
# and kubelet policies from becoming parallel test environments.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/ci/_deadline.sh"
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/ci/e2e_docker_lib.sh"

K3D_PLATFORM="${AWAKEN_K3D_PLATFORM:-linux/amd64}"
K3D_EXIT_CLEANUP_FUNCTION=""
K3D_BUILD_TIMEOUT_SECONDS="${AWAKEN_K3D_BUILD_TIMEOUT_SECONDS:-1800}"
K3D_DIAGNOSTIC_TIMEOUT_SECONDS="${AWAKEN_K3D_DIAGNOSTIC_TIMEOUT_SECONDS:-20}"

# One process-local creation receipt, not an inventory of other runs. Every
# actual scenario creates one cluster; a completed teardown releases the slot.
K3D_CREATED_CLUSTER=""
K3D_CREATED_OWNER=""
K3D_CREATED_RUN=""
K3D_CREATED_CONTAINER_IDS=()
K3D_CREATED_NETWORK_ID=""
K3D_CREATED_VOLUME_IDENTITIES=()
K3D_CREATED_SHARED_VOLUME=""
K3D_CREATED_IMAGE_VOLUME=""
K3D_CREATED_REGISTRY_ID=""
K3D_CURRENT_REGISTRY_ATTACHED=0
K3D_CREATION_CONFIRMED=0

k3d_validate_deadline() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

k3d_validate_image_digest() {
  [[ "$1" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]]
}

# Nested k3d nodes observe the host filesystem size. Produce one indivisible,
# absolute kubelet eviction policy so neither the trigger nor the recovery
# target scales with a large host filesystem. Callers still own capacity
# admission. Each output line is one kubelet setting for a k3d --k3s-arg.
# Offline fixtures import a bounded image set and use Never pull policy. Keep
# those images until cluster teardown: the default percentage image GC would
# evict them on a large, 85%-used host despite ample admitted free bytes.
# Kubelet documents 100 as disabling this GC trigger; absolute disk-pressure
# eviction and the pre-create/import byte+inode budgets remain independent.
k3d_eviction_policy() {
  local eviction="${1:-}"
  [[ "$eviction" =~ ^[1-9][0-9]*(Ki|Mi|Gi|Ti)$ ]] || {
    echo "invalid k3d eviction threshold: $eviction" >&2
    return 2
  }
  printf '%s\n' \
    "eviction-hard=imagefs.available<$eviction,nodefs.available<$eviction" \
    "eviction-minimum-reclaim=imagefs.available=$eviction,nodefs.available=$eviction" \
    "image-gc-high-threshold=100"
}

# Resolve the manifest identity of the image just built by the canonical image
# builder. Docker's config ID is not a registry/OCI manifest digest. Never
# synthesize a digest from a tag or substitute another published release.
k3d_image_digest() {
  local image="$1" digest
  k3d_validate_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" || return 2
  digest=$(run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    docker image inspect --format '{{json .RepoDigests}}' -- "$image" | python3 -c '
import json, sys
values = json.load(sys.stdin)
if not isinstance(values, list) or not values or not all(isinstance(v, str) for v in values):
    raise SystemExit("built image has no manifest digest; use a Docker containerd image store or supply AWAKEN_K8S_CONTROL_IMAGE")
print(sorted(set(values))[0])
') || return "$?"
  k3d_validate_image_digest "$digest" || {
    echo "Docker returned an invalid immutable image reference" >&2
    return 2
  }
  printf '%s\n' "$digest"
}

# Render an exact, caller-declared number of deployment-owned Kubernetes
# Sandbox image placeholders with one admitted immutable identity. Runtime
# rejects mutable control-forwarder images, so count drift must fail before
# Kubernetes sees any workload.
k3d_render_sandbox_image() {
  local overlay="${1:-}" image="${2:-}" expected_count="${3:-1}" manifest marker_count
  [[ -n "$overlay" && "$overlay" != -* ]] || return 2
  [[ "$expected_count" =~ ^[1-9][0-9]*$ ]] || return 2
  k3d_validate_image_digest "$image" || return 2
  manifest=$(kubectl kustomize "$overlay") || return "$?"
  marker_count=$( { grep -oF '__AWAKEN_SANDBOX_IMAGE__' <<<"$manifest" || true; } | wc -l)
  [[ "$marker_count" = "$expected_count" ]] || {
    echo "Kubernetes deployment Sandbox image pin count drifted" >&2
    return 2
  }
  printf '%s\n' "${manifest//__AWAKEN_SANDBOX_IMAGE__/$image}"
}

k3d_capacity_preflight() {
  local nodes="$1"
  shift
  # `capacity.py` owns the arithmetic. A caller with a non-default absolute
  # kubelet threshold passes `--eviction-threshold <quantity>` here so the same
  # plan retains both the trigger and minimum-reclaim margin after import.
  python3 "$(dirname "${BASH_SOURCE[0]}")/capacity.py" --nodes "$nodes" "$@"
}

# Read the eviction threshold from the node's actual k3s command. This keeps
# post-create image admission tied to the policy that kubelet is running,
# rather than asking scenario callers to repeat configuration from memory.
k3d_observed_eviction_threshold() {
  local cluster="${1:-}" observed="" hard_prefix="--kubelet-arg=eviction-hard=imagefs.available<"
  k3d_validate_name "$cluster" || return 2
  observed=$(docker inspect --format '{{range .Config.Cmd}}{{println .}}{{end}}' \
    -- "k3d-$cluster-server-0") || return "$?"

  local command hard="" minimum="" threshold="" hard_count=0 minimum_count=0
  local gc_high="" gc_high_count=0
  while IFS= read -r command; do
    case "$command" in
      --kubelet-arg=eviction-hard=*)
        hard_count=$((hard_count + 1))
        hard="$command"
        ;;
      --kubelet-arg=eviction-minimum-reclaim=*)
        minimum_count=$((minimum_count + 1))
        minimum="$command"
        ;;
      --kubelet-arg=image-gc-high-threshold=*)
        gc_high_count=$((gc_high_count + 1)); gc_high="$command" ;;
    esac
  done <<<"$observed"
  [[ "$hard_count" = 1 && "$minimum_count" = 1 && "$hard" == "$hard_prefix"* ]] || return 2
  threshold=${hard#"$hard_prefix"}
  threshold=${threshold%%,*}

  local expected=()
  mapfile -t expected < <(k3d_eviction_policy "$threshold") || return "$?"
  [[ "$hard" = "--kubelet-arg=${expected[0]}" \
    && "$minimum" = "--kubelet-arg=${expected[1]}" \
    && "$gc_high_count" = 1 \
    && "$gc_high" = "--kubelet-arg=${expected[2]}" ]] || return 2
  printf '%s\n' "$threshold"
}

k3d_ensure_images() {
  local image
  for image in "$@"; do
    docker image inspect --format '{{.Id}}' "$image" >/dev/null 2>&1 \
      || docker pull -q "$image" >/dev/null || return "$?"
  done
}

k3d_validate_name() {
  [[ ${#1} -le 32 && "$1" =~ ^[a-z0-9][a-z0-9-]*$ ]]
}

# Every scenario with unqualified kubectl calls uses this one private-context
# lifecycle. Install the caller-owned file before cluster creation, so k3d can
# never move the host's shared default context; bind its exact cluster after
# creation. The scenario still owns removal or explicit failed-run retention.
k3d_use_private_kubeconfig() {
  local path="${1:-}"
  [[ -n "$path" && "$path" != -* && ! -L "$path" ]] || return 2
  [[ ! -e "$path" || ( -f "$path" && ! -s "$path" ) ]] || return 2
  (umask 077; : > "$path") || return "$?"
  export KUBECONFIG="$path"
}

k3d_bind_private_kubeconfig() {
  local cluster="${1:-}"
  k3d_validate_name "$cluster" || return 2
  [[ -n "${KUBECONFIG:-}" && -f "$KUBECONFIG" && ! -L "$KUBECONFIG" ]] || return 2
  k3d kubeconfig get "$cluster" > "$KUBECONFIG"
}

# Validate that one exact Docker container is an ordinary server/agent node of
# the requested k3d cluster. Fault drivers must never accept a free-form Docker
# target: a typo or option-shaped value must stop before any network/lifecycle
# mutation, and the server load balancer is not a schedulable fault target.
k3d_validate_node_target() {
  local cluster="${1:-}" node="${2:-}" observed
  k3d_validate_name "$cluster" || return 2
  [[ "$node" =~ ^k3d-${cluster}-(server|agent)-[0-9]+$ ]] || return 2
  observed=$(docker inspect --format \
    '{{index .Config.Labels "k3d.cluster"}}|{{index .Config.Labels "k3d.role"}}' \
    -- "$node") || return "$?"
  [[ "$observed" = "$cluster|server" || "$observed" = "$cluster|agent" ]]
}

# A hard-stop receipt is physical evidence for this container namespace only.
# Never replace a node's Pods merely because Kubernetes lost its heartbeat.
k3d_hard_stop_node() {
  local cluster="${1:-}" node="${2:-}" container="${3:-}" observed kill_status=0 attempt deadline remaining
  [[ "$container" =~ ^[0-9a-f]{64}$ ]] || return 2
  k3d_validate_node_target "$cluster" "$node" || return "$?"
  observed=$(docker inspect --format '{{.Id}}|{{.State.Running}}' -- "$node") || return "$?"
  [[ "$observed" = "$container|true" ]] || return 2
  # The daemon may lose/delay its exit-event ACK after applying SIGKILL. Never
  # issue a second kill: join the same immutable object's physical result once.
  docker kill --signal KILL "$container" >/dev/null || kill_status=$?
  # Engine exit state can lag both the signal and a lost ACK. Read the same
  # immutable object within one ten-second budget; retries never resend KILL.
  deadline=$((SECONDS + 10))
  for attempt in {1..50}; do
    remaining=$((deadline - SECONDS))
    ((remaining > 0)) || break
    observed=$(timeout "${remaining}s" docker inspect --format '{{.State.Running}}|{{.State.ExitCode}}' -- "$container") || return "$?"
    [[ "$observed" != 'false|137' ]] || return 0
    [[ "$observed" == 'true|'* ]] || break
    [[ "$attempt" = 50 ]] || sleep 0.2
  done
  echo "exact node SIGKILL termination was not observed (kill status=$kill_status)" >&2
  [[ "$kill_status" = 0 ]] || return "$kill_status"
  return 2
}

# One exact inner container task, as distinct from the enclosing K3D node.
# Native Worker and PostgreSQL faults share this injection/observation owner.
k3d_hard_stop_cri_container() {
  local cluster="$1" node="$2" container="$3" observed kill_status=0 attempt deadline remaining
  [[ "$container" =~ ^[0-9a-f]{64}$ ]] || return 2
  k3d_validate_node_target "$cluster" "$node" || return "$?"
  observed=$(timeout 5 docker exec "$node" crictl inspect "$container" | jq -er '.status | .id + "|" + .state') || return "$?"
  [[ "$observed" = "$container|CONTAINER_RUNNING" ]] || return 2
  # StopContainer may send TERM, or lose its ACK after killing the process.
  # Request one unambiguous hard signal; never resend it after an uncertain ACK.
  timeout 15 docker exec "$node" ctr -n k8s.io tasks kill --signal SIGKILL --all "$container" >/dev/null || kill_status=$?
  deadline=$((SECONDS + 10))
  for attempt in {1..50}; do
    remaining=$((deadline - SECONDS))
    ((remaining > 0)) || break
    observed=$(timeout "${remaining}s" docker exec "$node" crictl inspect "$container" | jq -er '.status | .id + "|" + .state + "|" + (.exitCode | tostring)') || return "$?"
    [[ "$observed" != "$container|CONTAINER_EXITED|137" ]] || return 0
    [[ "$observed" = "$container|CONTAINER_RUNNING|"* ]] || break
    [[ "$attempt" = 50 ]] || sleep 0.2
  done
  echo "exact CRI SIGKILL termination was not observed (kill status=$kill_status)" >&2
  [[ "$kill_status" = 0 ]] || return "$kill_status"
  return 2
}

# One deliberate API fault for a caller-owned Pod. This returns only DELETE
# acceptance: never process closure, lost evidence, a successful Rebuild or a
# cleanup receipt. UID and RV come from the SAME original metadata observation.
# Conflicts/404/transport failures remain failures; no re-read or retry is added.
k3d_delete_exact_pod() {
  local cluster="${1:-}" namespace="${2:-}" pod="${3:-}" uid="${4:-}" version="${5:-}"
  k3d_validate_name "$cluster" || return 2
  [[ -n "$namespace" && -n "$pod" && -n "$uid" && -n "$version" \
    && "$namespace" != */* && "$pod" != */* ]] || return 2
  k3d_validate_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" || return 2
  # Reuse the existing bounded kubectl budget/standard client and JSON encoder.
  # No Bearer/key material or Pod response is printed by the fault primitive.
  python3 -c 'import json,sys; json.dump({"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":sys.argv[1],"resourceVersion":sys.argv[2]}},sys.stdout)' \
    "$uid" "$version" | run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl --context "k3d-$cluster" --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" \
    delete --raw "/api/v1/namespaces/$namespace/pods/$pod" -f - >/dev/null
}

# Capture the exact running node and underlay interface before the fault. Keep
# this receipt in the caller before any effect, so failed post-observation still
# leaves enough identity for cleanup. Docker disconnect destroys the interface;
# a packet filter instead preserves its IP, MAC, ifindex and installed routes.
k3d_node_network_identity() {
  local cluster="${1:-}" node="${2:-}" attachment addresses container
  k3d_validate_node_target "$cluster" "$node" || return "$?"
  attachment=$(docker inspect --format \
    "{{.Id}}|{{with index .NetworkSettings.Networks \"k3d-$cluster\"}}{{.IPAddress}}|{{.MacAddress}}{{end}}" \
    -- "$node") || return "$?"
  container=${attachment%%|*}
  [[ "$container" =~ ^[0-9a-f]{64}$ ]] || return 2
  # Docker names can be reused after inspection; immutable IDs cannot redirect
  # observation or the subsequent transaction onto a replacement incarnation.
  addresses=$(docker exec "$container" ip -j address show) || return "$?"
  python3 -c '
import ipaddress, json, re, sys
def require(condition, message):
    if not condition:
        raise SystemExit(message)
container, ip, mac = sys.argv[1].split("|")
require(re.fullmatch(r"[0-9a-f]{64}", container), "invalid container identity")
require(str(ipaddress.IPv4Address(ip)) == ip, "invalid IPv4 identity")
require(re.fullmatch(r"[0-9a-f]{2}(:[0-9a-f]{2}){5}", mac), "invalid MAC identity")
links = [link for link in json.load(sys.stdin)
         if link.get("address") == mac and any(
             address.get("family") == "inet" and address.get("local") == ip
             for address in link.get("addr_info", []))]
require(len(links) == 1, "underlay interface is absent or ambiguous")
link = links[0]
require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,14}", link["ifname"]), "invalid interface")
require(type(link["ifindex"]) is int and link["ifindex"] > 0, "invalid ifindex")
require("UP" in link["flags"], "underlay is not administratively up")
print("|".join([container, link["ifname"], str(link["ifindex"]), ip, mac]))
' "$attachment" <<<"$addresses"
}

# One owned rule set for observation, installation and exact removal. Direction
# is a packet property; ingress-only and egress-only must not silently become a
# bidirectional fault. Interface identity is validated by the receipt owner.
k3d_node_partition_rules() {
  local interface="$1" direction="${2:-both}"
  [[ "$direction" = both || "$direction" = ingress || "$direction" = egress ]] || return 2
  if [[ "$direction" != egress ]]; then
    printf '%s\n' "-A INPUT -i $interface -j AWAKEN-K3D-PARTITION" \
      "-A FORWARD -i $interface -j AWAKEN-K3D-PARTITION"
  fi
  if [[ "$direction" != ingress ]]; then
    printf '%s\n' "-A OUTPUT -o $interface -j AWAKEN-K3D-PARTITION" \
      "-A FORWARD -o $interface -j AWAKEN-K3D-PARTITION"
  fi
}

k3d_node_network_partition_state() {
  local node="$1" interface="$2" direction="${3:-both}" rules expected owned
  owned=$(k3d_node_partition_rules "$interface" "$direction") || return "$?"
  rules=$(docker exec "$node" iptables-save -t filter) || return "$?"
  rules=$(printf '%s\n' "$rules" | { grep -F AWAKEN-K3D-PARTITION || true; } | sort)
  [[ -n "$rules" ]] || { printf 'open\n'; return 0; }
  expected=$(printf '%s\n' ':AWAKEN-K3D-PARTITION - [0:0]' \
    '-A AWAKEN-K3D-PARTITION -j DROP' "$owned" | sort)
  [[ "$rules" = "$expected" ]] || {
    echo "node partition rules differ from the exact owned fault" >&2
    return 2
  }
  printf 'blocked\n'
}

# One atomic filter-table transaction blocks both local and forwarded underlay
# packets inside the exact node namespace. Never flush Kubernetes rules or
# recreate a network attachment. Reopening is idempotent for cleanup, but an
# unexpected container/interface identity or changed fault rule fails closed.
k3d_set_node_network_partition() {
  local cluster="${1:-}" node="${2:-}" receipt="${3:-}" desired="${4:-}"
  local observed interface state payload receipt_container receipt_remainder owned direction="${5:-both}"
  [[ "$direction" = both || "$direction" = ingress || "$direction" = egress ]] || return 2
  [[ "$desired" = blocked || "$desired" = open ]] || return 2
  [[ -n "$receipt" ]] || return 2
  observed=$(k3d_node_network_identity "$cluster" "$node") || return "$?"
  [[ "$observed" = "$receipt" ]] || {
    echo "node network identity changed since fault preparation" >&2
    return 2
  }
  IFS='|' read -r receipt_container interface receipt_remainder <<<"$receipt"
  state=$(k3d_node_network_partition_state "$receipt_container" "$interface" "$direction") || return "$?"
  owned=$(k3d_node_partition_rules "$interface" "$direction") || return "$?"
  if [[ "$desired" = blocked ]]; then
    [[ "$state" = open ]] || return 2
    payload="*filter
:AWAKEN-K3D-PARTITION - [0:0]
-A AWAKEN-K3D-PARTITION -j DROP
$(sed -E 's/^-A ([^ ]+) /-I \1 1 /' <<<"$owned")
COMMIT"
  else
    [[ "$state" != open ]] || return 0
    payload="*filter
$(sed 's/^-A /-D /' <<<"$owned")
-F AWAKEN-K3D-PARTITION
-X AWAKEN-K3D-PARTITION
COMMIT"
  fi
  docker exec -i "$receipt_container" iptables-restore --noflush <<<"$payload" || return "$?"
  state=$(k3d_node_network_partition_state "$receipt_container" "$interface" "$direction") || return "$?"
  [[ "$state" = "$desired" ]]
}

# Community-owned node image preparation. A frozen chart/image is rendered
# and installed only in the explicitly named fixture cluster. P1 bad target or
# chart digest => no mutation; P2 successful install => real controller/daemon
# readiness; P3 missing backend => fail, never replace with a fake cache.
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
  k3d_validate_deadline "$K3D_BUILD_TIMEOUT_SECONDS" || {
    echo "AWAKEN_K3D_BUILD_TIMEOUT_SECONDS must be a positive integer" >&2
    return 2
  }
  local -a build_options=()
  if [[ -n "${AWAKEN_K3D_BUILD_NETWORK:-}" ]]; then
    build_options+=(--network "$AWAKEN_K3D_BUILD_NETWORK")
  fi
  if [[ -n "${AWAKEN_K3D_BUILD_CA_BUNDLE:-}" ]]; then
    if [[ ! -s "$AWAKEN_K3D_BUILD_CA_BUNDLE" ]]; then
      echo "AWAKEN_K3D_BUILD_CA_BUNDLE must name a nonempty readable file" >&2
      return 2
    fi
    build_options+=(--secret "id=k3d_apt_ca,src=$AWAKEN_K3D_BUILD_CA_BUNDLE")
  fi
  run_with_deadline "$K3D_BUILD_TIMEOUT_SECONDS" docker build "${build_options[@]}" "$@"
}

# Diagnostics are observations, not another lifecycle owner. Bind each probe
# to the exact cluster context and a whole-process deadline so a failed build or
# unavailable API cannot turn the original failure into an ambient, unbounded
# kubectl session.
k3d_diagnostic_kubectl() {
  local cluster="$1"
  shift
  k3d_validate_name "$cluster" || return 2
  k3d_validate_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" || {
    echo "AWAKEN_K3D_DIAGNOSTIC_TIMEOUT_SECONDS must be a positive integer" >&2
    return 2
  }
  run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" \
    kubectl --context "k3d-$cluster" \
    --request-timeout="${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s" "$@"
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

# Read only the selected cluster's immutable IDs and the three ownership labels.
# Any IO or malformed identity is unknown, never an empty/absent observation.
_k3d_cluster_members() {
  local cluster="$1" ids id observed
  local records=()
  ids=$(docker ps -aq --no-trunc --filter "label=k3d.cluster=$cluster") || return "$?"
  for id in $ids; do
    [[ "$id" =~ ^[0-9a-f]{64}$ ]] || return 2
    observed=$(docker inspect --format \
      '{{.Id}}|{{index .Config.Labels "awaken.e2e.owner"}}|{{index .Config.Labels "awaken.e2e.run"}}|{{index .Config.Labels "k3d.cluster"}}|{{index .Config.Labels "k3d.cluster.network.id"}}|{{index .Config.Labels "k3d.cluster.network.external"}}|{{with index .Config.Labels "k3d.cluster.imageVolume"}}{{.}}{{end}}' \
      -- "$id") || return "$?"
    [[ "$observed" = "$id|"* && "$observed" != *$'\n'* ]] || return 2
    records+=("$observed")
  done
  (( ${#records[@]} == 0 )) || printf '%s\n' "${records[@]}" | sort -u
}

_k3d_capture_creation() {
  local members id owner run cluster network external image extra observed names name identity retained found managed
  members=$(_k3d_cluster_members "$K3D_CREATED_CLUSTER") || return "$?"
  [[ -n "$members" ]] || return 2
  K3D_CREATED_CONTAINER_IDS=()
  K3D_CREATED_VOLUME_IDENTITIES=()
  while IFS='|' read -r id owner run cluster network external image extra; do
    [[ -z "$K3D_CREATED_REGISTRY_ID" || "$id" != "$K3D_CREATED_REGISTRY_ID" ]] || continue
    [[ "$owner" = "$K3D_CREATED_OWNER" && "$run" = "$K3D_CREATED_RUN" \
      && "$cluster" = "$K3D_CREATED_CLUSTER" && "$network" =~ ^[0-9a-f]{64}$ \
      && "$external" = false && "$image" = "$K3D_CREATED_IMAGE_VOLUME" && -z "$extra" ]] || return 2
    if [[ -z "$K3D_CREATED_NETWORK_ID" ]]; then K3D_CREATED_NETWORK_ID="$network"; fi
    [[ "$network" = "$K3D_CREATED_NETWORK_ID" ]] || return 2
    K3D_CREATED_CONTAINER_IDS+=("$id")
  done <<<"$members"
  (( ${#K3D_CREATED_CONTAINER_IDS[@]} > 0 )) || return 2
  # The pinned K3D creator records both the actual network ID and whether it
  # created or reused it in node labels. Only its external=false creation fact can
  # authorize this network; a later name lookup is not creation evidence.
  observed=$(docker network inspect --format '{{.Id}}|{{.Name}}' -- "$K3D_CREATED_NETWORK_ID") || return "$?"
  [[ "$observed" = "$K3D_CREATED_NETWORK_ID|k3d-$K3D_CREATED_CLUSTER" ]] || return 2
  observed=$(_e2e_docker_owned_volume "$K3D_CREATED_IMAGE_VOLUME") || return "$?"
  [[ -n "$observed" ]] || return 2
  K3D_CREATED_VOLUME_IDENTITIES=("$observed")
  for id in "${K3D_CREATED_CONTAINER_IDS[@]}"; do
    names=$(docker inspect --format '{{range .Mounts}}{{if eq .Type "volume"}}{{println .Name}}{{end}}{{end}}' -- "$id") || return "$?"
    for name in $names; do
      # The closed creator passes only its checked shared volume. The other
      # volume mounts are Docker-generated anonymous volumes of these new CIDs.
      [[ "$name" = "$K3D_CREATED_IMAGE_VOLUME" || "$name" =~ ^[0-9a-f]{64}$ ]] || return 2
      identity=$(docker volume inspect --format \
        '{{.Name}}|{{.CreatedAt}}|{{with index .Labels "awaken.e2e.owner"}}{{.}}{{end}}|{{with index .Labels "awaken.e2e.run"}}{{.}}{{end}}|{{with index .Labels "awaken.e2e.creation"}}{{.}}{{end}}' "$name") || return "$?"
      if [[ "$name" = "$K3D_CREATED_IMAGE_VOLUME" || "$name" = "$K3D_CREATED_SHARED_VOLUME" ]]; then
        observed=$(_e2e_docker_owned_volume "$name") || return "$?"
        [[ "$observed" = "$identity" ]] || return 2
      fi
      [[ "$identity" = "$name|"?* && "$identity" != *$'\n'* ]] || return 2
      found=0
      for retained in "${K3D_CREATED_VOLUME_IDENTITIES[@]}"; do [[ "$identity" != "$retained" ]] || found=1; done
      [[ "$found" = 1 ]] || K3D_CREATED_VOLUME_IDENTITIES+=("$identity")
    done
  done
  _k3d_check_creation || return "$?"
  K3D_CREATION_CONFIRMED=1
}

# Validate the retained creation receipt before the existing K3D teardown. No
# replacement/unlabelled/foreign member or foreign volume borrower is removed,
# even if some other members are ours. The whole mixed group stays for diagnosis.
_k3d_check_creation() {
  local members id owner run cluster creator_network external image extra retained found network volume borrowers borrower present observed managed
  K3D_CURRENT_REGISTRY_ATTACHED=0
  members=$(_k3d_cluster_members "$K3D_CREATED_CLUSTER") || return "$?"
  if [[ -n "$members" ]]; then
    while IFS='|' read -r id owner run cluster creator_network external image extra; do
      [[ -z "$K3D_CREATED_REGISTRY_ID" || "$id" != "$K3D_CREATED_REGISTRY_ID" ]] || continue
      [[ "$owner" = "$K3D_CREATED_OWNER" && "$run" = "$K3D_CREATED_RUN" \
        && "$cluster" = "$K3D_CREATED_CLUSTER" && "$creator_network" = "$K3D_CREATED_NETWORK_ID" \
        && "$external" = false && "$image" = "$K3D_CREATED_IMAGE_VOLUME" && -z "$extra" ]] || return 2
      found=0
      for retained in "${K3D_CREATED_CONTAINER_IDS[@]}"; do [[ "$id" != "$retained" ]] || found=1; done
      [[ "$found" = 1 ]] || return 2
    done <<<"$members"
  fi
  network=$(docker network ls --no-trunc --filter "name=^k3d-${K3D_CREATED_CLUSTER}$" --format '{{.ID}}') || return "$?"
  [[ -z "$network" || "$network" = "$K3D_CREATED_NETWORK_ID" ]] || return 2
  if [[ -n "$network" ]]; then
    borrowers=$(docker network inspect --format '{{range $id, $member := .Containers}}{{println $id}}{{end}}' \
      -- "$network") || return "$?"
    for borrower in $borrowers; do
      if [[ -n "$K3D_CREATED_REGISTRY_ID" && "$borrower" = "$K3D_CREATED_REGISTRY_ID" ]]; then
        K3D_CURRENT_REGISTRY_ATTACHED=1
        continue
      fi
      found=0
      for retained in "${K3D_CREATED_CONTAINER_IDS[@]}"; do [[ "$borrower" != "$retained" ]] || found=1; done
      [[ "$found" = 1 ]] || return 2
    done
  fi
  # Labels cannot authorize a managed volume. An unexpected object in the
  # old K3D deletion domain is retained; no domain enumeration feeds deletion.
  managed=$(docker volume ls --filter label=app=k3d --filter "label=k3d.cluster=$K3D_CREATED_CLUSTER" --format '{{.Name}}') || return "$?"
  for volume in $managed; do
    found=0
    for retained in "${K3D_CREATED_VOLUME_IDENTITIES[@]}"; do [[ "${retained%%|*}" != "$volume" ]] || found=1; done
    [[ "$found" = 1 ]] || return 2
  done
  # These creation-time identities survive partial removal of the node that
  # mounted them. Missing nodes cannot erase a foreign borrower's evidence.
  for retained in "${K3D_CREATED_VOLUME_IDENTITIES[@]}"; do
    observed=$(_e2e_docker_check_created_volume "$retained") || return "$?"
    [[ -n "$observed" ]] || continue
    volume=${retained%%|*}
    _e2e_docker_check_volume_mounts "$volume" "${K3D_CREATED_CONTAINER_IDS[@]}" || return "$?"
  done

}

# Resolve the existing storage-cleanup node to a retained immutable container,
# never execute a cleanup command against an unverified replacement by name.
_k3d_created_node_id() {
  local node="$1" id retained
  [[ "$K3D_CREATION_CONFIRMED" = 1 \
    && "$node" =~ ^k3d-${K3D_CREATED_CLUSTER}-(server|agent)-[0-9]+$ ]] || return 2
  id=$(docker inspect --format '{{.Id}}' -- "$node") || return "$?"
  for retained in "${K3D_CREATED_CONTAINER_IDS[@]}"; do
    if [[ "$id" = "$retained" ]]; then printf '%s\n' "$id"; return 0; fi
  done
  return 2
}

k3d_delete_cluster() {
  local cluster="$1"
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  # An uncertain volume-create response is retained as an incomplete creation,
  # not a successful empty cleanup. No object is deleted on that evidence.
  [[ -z "$E2E_UNCONFIRMED_VOLUME" ]] || {
    echo "unconfirmed volume creation retained: $E2E_UNCONFIRMED_VOLUME" >&2
    return 2
  }
  # An early EXIT trap owns no cluster merely because it knows its name.
  [[ "$cluster" = "$K3D_CREATED_CLUSTER" ]] || return 0
  [[ "$K3D_CREATION_CONFIRMED" = 1 ]] || {
    echo "unconfirmed cluster creation retained: $cluster" >&2
    return 2
  }
  _k3d_check_creation || return "$?"
  if [[ "$K3D_CURRENT_REGISTRY_ATTACHED" = 1 ]]; then
    # Undo this creator's exact attachment only. The external Registry itself
    # belongs to its existing owner and is never removed by cluster cleanup.
    docker network disconnect "$K3D_CREATED_NETWORK_ID" "$K3D_CREATED_REGISTRY_ID" || return "$?"
    _k3d_check_creation || return "$?"
    [[ "$K3D_CURRENT_REGISTRY_ATTACHED" = 0 ]] || return 2
  fi
  local id present retained volume observed
  # Do not hand a validated name back to K3D for a SECOND selection. Docker's
  # immutable IDs cannot redirect these effects onto a replacement incarnation.
  for id in "${K3D_CREATED_CONTAINER_IDS[@]}"; do
    present=$(docker ps -aq --no-trunc --filter "id=$id") || return "$?"
    [[ -n "$present" ]] || continue
    [[ "$present" = "$id" ]] || return 2
    docker rm -f -v -- "$id" >/dev/null || return "$?"
  done
  for retained in "${K3D_CREATED_VOLUME_IDENTITIES[@]}"; do
    observed=$(_e2e_docker_check_created_volume "$retained") || return "$?"
    [[ -n "$observed" ]] || continue
    volume=${retained%%|*}
    # The caller retains its explicitly supplied shared data. This lifecycle
    # owns its selected images volume and anonymous volumes created with nodes.
    if [[ "$volume" = "$K3D_CREATED_SHARED_VOLUME" ]]; then
      _e2e_docker_check_volume_mounts "$volume" || return "$?"
      continue
    fi
    _e2e_docker_remove_created_volume "$observed" || return "$?"
  done
  present=$(docker network ls --no-trunc --filter "id=$K3D_CREATED_NETWORK_ID" --format '{{.ID}}') || return "$?"
  if [[ -n "$present" ]]; then
    [[ "$present" = "$K3D_CREATED_NETWORK_ID" ]] || return 2
    # A late foreign endpoint makes this non-force operation fail. It is never
    # removed/disconnected merely to make our network removable.
    docker network rm -- "$K3D_CREATED_NETWORK_ID" >/dev/null || return "$?"
  fi
  local observation_status=0
  _k3d_docker_residue "$cluster" || observation_status=$?
  case "$observation_status" in
    0) echo "cluster coordinate still has Docker residue: $cluster" >&2; return 1 ;;
    1) ;;
    *) echo "cannot verify Docker cleanup: $cluster" >&2; return "$observation_status" ;;
  esac
  K3D_CREATED_CLUSTER=""
  K3D_CREATION_CONFIRMED=0
  return 0
}

# Delete one exact cluster and remove its caller-owned local artifacts even when
# the cluster postcondition fails. The cluster failure remains the return value:
# local cleanup must not hide infrastructure residue, and `set -e` must not leave
# copied binaries or an anonymous kubeconfig merely because that residue exists.
k3d_delete_cluster_and_remove() {
  local cluster="$1"
  shift
  local cluster_status=0 remove_status=0 held_receipt=0 observation_status=0
  [[ "$K3D_CREATED_CLUSTER" != "$cluster" ]] || held_receipt=1
  k3d_delete_cluster "$cluster" || cluster_status=$?
  if (( $# > 0 )); then
    rm -f -- "$@" || remove_status=$?
  fi
  [[ "$cluster_status" = 0 ]] || return "$cluster_status"
  [[ "$remove_status" = 0 ]] || return "$remove_status"
  # A private kubeconfig is a caller artifact, not a Docker resource. Observe
  # the complete coordinate only AFTER that existing owner removes its files.
  [[ "$held_receipt" = 1 ]] || return 0
  k3d_cluster_residue "$cluster" || observation_status=$?
  case "$observation_status" in
    1) return 0 ;;
    0) return 1 ;;
    *) return "$observation_status" ;;
  esac
}

# Install one process-level EXIT owner around a scenario cleanup function. Bash
# otherwise preserves the scenario body's successful status when an EXIT trap
# merely returns non-zero, which can turn exact cluster residue into a false
# green. The cleanup receives the body status as its sole argument; cleanup
# failure takes precedence, while successful cleanup preserves the body status.
k3d_install_exit_cleanup() {
  local cleanup_function="${1:-}"
  [[ "$cleanup_function" =~ ^[a-zA-Z_][a-zA-Z0-9_]*$ ]] \
    && declare -F "$cleanup_function" >/dev/null || {
    echo "invalid k3d EXIT cleanup function: $cleanup_function" >&2
    return 2
  }
  K3D_EXIT_CLEANUP_FUNCTION="$cleanup_function"
  trap 'k3d_run_exit_cleanup "$?"' EXIT
  # A signal must run the EXIT cleanup too, or the cluster and its volumes stay.
  trap 'exit 130' INT
  trap 'exit 143' TERM
}

k3d_run_exit_cleanup() {
  local body_status="$1" cleanup_status=0
  trap - EXIT
  "$K3D_EXIT_CLEANUP_FUNCTION" "$body_status" || cleanup_status=$?
  if (( cleanup_status != 0 )); then
    exit "$cleanup_status"
  fi
  exit "$body_status"
}

# Observe a validated coordinate without authorizing deletion: residue can be
# an unrelated collision. The Docker part serves its teardown; the complete
# observer includes the caller's private context after artifact cleanup. It
# never prunes or mutates unrelated state. Return 0 when any residue exists,
# 1 only when every owner confirms absence, and 2 when an owner cannot be read.
_k3d_docker_residue() {
  local cluster="$1" network="k3d-$1"
  local images_volume="k3d-$1-images" observed
  command -v k3d >/dev/null 2>&1 \
    && command -v docker >/dev/null 2>&1 || return 2

  # Exact-name queries use a non-zero exit both for "absent" and for observer
  # failure in k3d/kubectl. Read each owner once without a name filter, then
  # distinguish confirmed absence from a failed owner read ourselves.
  if ! observed=$(k3d cluster list --no-headers 2>/dev/null); then
    return 2
  fi
  if awk -v expected="$cluster" '$1 == expected { found = 1 } END { exit !found }' \
    <<<"$observed"; then
    return 0
  fi

  if ! observed=$(docker network ls --filter "name=^${network}$" --format '{{.Name}}' 2>/dev/null); then
    return 2
  fi
  if awk -v expected="$network" '$0 == expected { found = 1 } END { exit !found }' \
    <<<"$observed"; then
    return 0
  fi

  if ! observed=$(docker ps -a --filter "name=^k3d-${cluster}-" --format '{{.Names}}' 2>/dev/null); then
    return 2
  fi
  if awk -v prefix="k3d-${cluster}-" 'index($0, prefix) == 1 { found = 1 } END { exit !found }' \
    <<<"$observed"; then
    return 0
  fi

  if ! observed=$(docker volume ls --filter "name=^${images_volume}$" --format '{{.Name}}' 2>/dev/null); then
    return 2
  fi
  if awk -v expected="$images_volume" '$0 == expected { found = 1 } END { exit !found }' \
    <<<"$observed"; then
    return 0
  fi

  if ! observed=$(docker volume ls --filter label=app=k3d --filter "label=k3d.cluster=$cluster" --format '{{.Name}}' 2>/dev/null); then
    return 2
  fi
  [[ -z "$observed" ]] || return 0
  return 1
}

k3d_cluster_residue() {
  local cluster="$1" context="k3d-$1" observed status=0
  _k3d_docker_residue "$cluster" || status=$?
  [[ "$status" = 1 ]] || return "$status"
  command -v kubectl >/dev/null 2>&1 || return 2
  if ! observed=$(kubectl config get-contexts --no-headers 2>/dev/null); then
    return 2
  fi
  if awk -v expected="$context" '$1 == expected || $2 == expected { found = 1 } END { exit !found }' \
    <<<"$observed"; then
    return 0
  fi
  return 1
}

# k3d_create_cluster <name> <agent-count> [eviction-threshold] [registry-coordinate] [--shared-volume docker-volume-id] [images...]
#
# Nested nodes see the host's total filesystem size. One absolute default keeps
# a large host from tainting an otherwise admitted fixture. Actual byte/inode
# budgets remain independently enforced before create and image import.
# Explicit overrides are absolute Kubernetes quantities, never percentages.
k3d_create_cluster() {
  local cluster="$1" agents="$2" eviction="${3:-128Mi}" registry="${4:-}"
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  # The qualification topology is bounded to 64 nodes. Validate before shell
  # arithmetic so oversized or octal-shaped input cannot wrap the disk budget.
  [[ "$agents" =~ ^(0|[1-9][0-9]?)$ ]] && (( agents < 64 )) || {
    echo "invalid k3d agent count: $agents" >&2
    return 2
  }
  local eviction_policy_text eviction_policy
  eviction_policy_text=$(k3d_eviction_policy "$eviction") || return "$?"
  mapfile -t eviction_policy <<<"$eviction_policy_text"
  [[ -z "$registry" || "$registry" =~ ^[a-zA-Z0-9.-]+:[0-9]+$ ]] || {
    echo "invalid k3d Registry coordinate: $registry" >&2
    return 2
  }
  local images=("${@:5}") shared_volume=""
  if [[ "${images[0]:-}" = "--shared-volume" ]]; then
    shared_volume="${images[1]:-}"
    # Only this invocation's checked, Docker-generated volume is accepted.
    # A missing source cannot silently create a bind directory or named volume.
    local shared_identity
    [[ "$shared_volume" =~ ^[0-9a-f]{64}$ ]] || return 2
    shared_identity=$(_e2e_docker_owned_volume "$shared_volume") || return "$?"
    [[ -n "$shared_identity" ]] || {
      echo "missing shared continuation volume" >&2
      return 2
    }
    images=("${images[@]:2}")
  fi
  k3d_capacity_preflight "$((agents + 1))" \
    --eviction-threshold "$eviction" "${images[@]}" || return "$?"
  local collision=0
  k3d_cluster_residue "$cluster" || collision=$?
  [[ "$collision" = 1 ]] || {
    echo "existing or unobservable cluster coordinate; refusing replacement: $cluster" >&2
    return 2
  }
  if [[ -n "$K3D_CREATED_CLUSTER" ]]; then
    echo "a previous cluster creation receipt is still held: $K3D_CREATED_CLUSTER" >&2
    return 2
  fi
  local registry_id=""
  if [[ -n "$registry" ]]; then
    registry_id=$(docker inspect --format '{{.Id}}' -- "${registry%:*}") || return "$?"
    [[ "$registry_id" =~ ^[0-9a-f]{64}$ ]] || return 2
  fi
  [[ -n "${E2E_RUN_ID:-}" ]] || e2e_docker_init k3d || return "$?"
  # Pinned K3D requires this empty volume while ensuring its tools node, even
  # when image import itself streams directly. Select it through the SAME
  # checked creator; K3D's subsequent ensure must consume this exact receipt.
  e2e_docker_create_volume --name "k3d-$cluster-images" "awaken.e2e.cluster=$cluster" || return "$?"
  local image_volume="$E2E_CREATED_VOLUME"
  K3D_CREATED_CLUSTER="$cluster"
  K3D_CREATED_OWNER="$E2E_OWNER"
  K3D_CREATED_RUN="$E2E_RUN_ID"
  K3D_CREATED_CONTAINER_IDS=()
  K3D_CREATED_NETWORK_ID=""
  K3D_CREATED_VOLUME_IDENTITIES=()
  K3D_CREATED_SHARED_VOLUME="$shared_volume"
  K3D_CREATED_IMAGE_VOLUME="$image_volume"
  K3D_CREATED_REGISTRY_ID="$registry_id"
  K3D_CREATION_CONFIRMED=0
  local args=(
    # Lifecycle decision table: C1 create succeeds -> the scenario cleanup owns
    # normal teardown; C2 create fails + caller keeps failures -> retain the
    # exact partial nodes for diagnostics; C3 create fails + caller cleans ->
    # the same cleanup owner deletes every exact residue.  K3D auto-rollback
    # would race C2/C3 and erase node logs before either policy can observe them.
    cluster create "$cluster" --agents "$agents" --wait --timeout 180s --no-rollback
    --runtime-label "$E2E_OWNER_LABEL=$E2E_OWNER@all"
    --runtime-label "$E2E_RUN_LABEL=$E2E_RUN_ID@all"
    --runtime-ulimit "nofile=65536:65536"
    # Repository scenarios install and import every component they exercise.
    # Disable the unused bundled controllers so an offline proof run cannot
    # contend with background registry retries for images outside its fixture.
    --k3s-arg "--disable=traefik@server:*"
    --k3s-arg "--disable=metrics-server@server:*"
  )
  local setting
  for setting in "${eviction_policy[@]}"; do
    args+=(--k3s-arg "--kubelet-arg=$setting@server:*")
  done
  if (( agents > 0 )); then
    for setting in "${eviction_policy[@]}"; do
      args+=(--k3s-arg "--kubelet-arg=$setting@agent:*")
    done
  fi
  [[ -z "$registry" ]] || args+=(--registry-use "$registry")
  [[ -z "$shared_volume" ]] || args+=(--volume "$shared_volume:/var/lib/awaken-continuation@all")
  local create_status=0 receipt_status=0
  k3d "${args[@]}" || create_status=$?
  _k3d_capture_creation || receipt_status=$?
  [[ "$create_status" = 0 ]] || return "$create_status"
  return "$receipt_status"
}

# Load the installed provisioner's exact dependency images through the one
# offline importer. Both retained-volume suites use this owner and readiness.
k3d_import_storage_images() {
  local cluster="$1"
  shift
  k3d_validate_name "$cluster" || {
    echo "invalid k3d cluster name: $cluster" >&2
    return 2
  }
  local context="k3d-$cluster"
  local provisioner_image="$(kubectl --context "$context" -n kube-system get deploy local-path-provisioner \
    -o jsonpath='{.spec.template.spec.containers[0].image}')"
  [ -n "$provisioner_image" ] || {
    echo "local-path provisioner deployment has no image" >&2
    return 1
  }
  local helper_image="$(
    kubectl --context "$context" -n kube-system get configmap local-path-config \
      -o jsonpath='{.data.helperPod\.yaml}' \
      | awk '$1 == "image:" { print $2; exit }' \
      | tr -d '"'
  )"
  [ -n "$helper_image" ] || {
    echo "local-path provisioner configuration has no helper image" >&2
    return 1
  }
  k3d_import_images \
    "$cluster" \
    "$@" \
    "$provisioner_image" \
    "$helper_image" || return "$?"
  # A provisioner Pod can enter registry backoff before the offline image import
  # completes. Recreate that disposable system Pod so it observes the imported
  # image immediately, then fence the PVC tests on actual provisioner readiness.
  kubectl --context "$context" -n kube-system rollout restart deploy/local-path-provisioner >/dev/null || return "$?"
  kubectl --context "$context" -n kube-system rollout status deploy/local-path-provisioner --timeout=120s
}

# The storage chart is the sole image/config owner. Fixture provisioning uses
# real loop-backed ext4 filesystems inside its unique Node container; ordinary
# application PVCs never allocate a helper Pod on the static class.
k3d_local_storage_image() {
  local image
  image="$(awk '$1 == "image:" {print $2; exit}' "$(dirname "${BASH_SOURCE[0]}")/../../deploy/storage/local-pv/values.yaml")"
  k3d_validate_image_digest "$image" || return 2
  printf '%s\n' "$image"
}

k3d_prepare_local_storage() {
  local cluster="$1" count="${2:-4}" node="k3d-$1-server-0" image
  k3d_validate_name "$cluster" || return 2
  [[ "$count" =~ ^[1-9][0-9]?$ ]] && (( count <= 16 )) || return 2
  [[ "$K3D_PLATFORM" = linux/amd64 ]] || return 2
  image="$(k3d_local_storage_image)" || return "$?"
  # Never reuse/format an existing storage directory, including a prior failed
  # fixture's volumes. Unique fresh cluster lifecycle remains the outer owner.
  docker exec "$node" sh -ec 'test ! -e /mnt/awaken-local && test ! -e /mnt/awaken-local-disks; mkdir -p /mnt; mkdir -m 0700 /mnt/awaken-local /mnt/awaken-local-disks' || return "$?"
  docker run --rm --network none --privileged --pid "container:$node" \
    --entrypoint /bin/sh "$image" -ec '
      count=$1; index=0; base=/proc/1/root/mnt; loop=
      trap '\''test -z "$loop" || losetup -d "$loop"'\'' EXIT
      while [ "$index" -lt "$count" ]; do
        disk="$base/awaken-local-disks/volume-$index.img"
        target="$base/awaken-local/volume-$index"
        test ! -e "$disk"; test ! -e "$target"
        truncate -s 2G "$disk"
        mkfs.ext4 -q -F "$disk"
        mkdir -m 0770 "$target"
        # Docker snapshots /dev when this formatter starts. LOOP_CTL_GET_FREE
        # can allocate a newer device whose node is not in that snapshot yet.
        device=$(losetup --find); minor=${device#/dev/loop}
        test -e "$device" || mknod "$device" b 7 "$minor"
        loop=$(losetup --find --show "$disk")
        minor=${loop#/dev/loop}
        nsenter -t 1 -m -- /bin/sh -ec '\''
          device=$1; minor=$2; target=$3
          test -e "$device" || mknod "$device" b 7 "$minor"
          /bin/aux/mount -o nodev,nosuid "$device" "$target"
        '\'' awaken-local-mount "$loop" "$minor" "/mnt/awaken-local/volume-$index"
        # Detaching a mounted loop sets AUTOCLEAR; the owned mount keeps it
        # live and final fixture unmount releases it without host cleanup.
        losetup -d "$loop"; loop=
        index=$((index+1))
      done
    ' awaken-local-disks "$count" || return "$?"
  kubectl --context "k3d-$cluster" label node "$node" awaken.dev/local-storage=true --overwrite >/dev/null
}

k3d_install_local_storage() {
  local cluster="$1" output="$2" count="${3:-4}" image
  image="$(k3d_local_storage_image)" || return "$?"
  k3d_ensure_images "$image" || return "$?"
  k3d_import_images "$cluster" "$image" || return "$?"
  k3d_prepare_local_storage "$cluster" "$count" || return "$?"
  bash "$(dirname "${BASH_SOURCE[0]}")/../../deploy/storage/local-pv/install.sh" --context "k3d-$cluster" "$output"
}

# Kubernetes adapter publication owns the opaque Session scope annotation.
# Never infer Session ownership from a Pod name, iteration order or copied files.
k3d_sandbox_pod_for_session() {
  local namespace="$1" session="$2"
  kubectl -n "$namespace" get pods -l app=awaken-sandbox -o json | python3 -c '
import json, sys
session = sys.argv[1]
pods = [pod for pod in json.load(sys.stdin)["items"]
        if pod.get("metadata", {}).get("annotations", {}).get("awaken.dev/sandbox-scope") == session]
if not session or len(pods) != 1:
    raise SystemExit("expected exactly one Sandbox Pod for Session " + session)
metadata = pods[0]["metadata"]
if metadata.get("deletionTimestamp") or not metadata.get("name") or not metadata.get("uid"):
    raise SystemExit("Session Sandbox Pod is deleting or lacks physical identity")
print(metadata["name"])
' "$session"
}

k3d_archive_name() {
  local value="${1//[^a-zA-Z0-9_.-]/_}"
  printf '%s' "$value"
}

k3d_export_image() {
  python3 "$(dirname "${BASH_SOURCE[0]}")/image_archive.py" \
    --platform "$K3D_PLATFORM" --output "$2" "$1"
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
  local context="k3d-$cluster"
  local pause_image
  pause_image=$(docker exec "$node" sh -c 'grep -hoE "sandbox_image = \"[^\"]+\"" /var/lib/rancher/k3s/agent/etc/containerd/config.toml* 2>/dev/null | head -1 | cut -d\" -f2' 2>/dev/null || true)
  pause_image=${pause_image:-rancher/mirrored-pause:3.6}
  local coredns_image
  coredns_image=$(kubectl --context "$context" -n kube-system get deploy coredns -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
  coredns_image=${coredns_image:-rancher/mirrored-coredns-coredns:1.10.1}

  local images=("$pause_image" "$coredns_image" "$@")
  local unique=() image existing candidate_index
  for image in "${images[@]}"; do
    existing=0
    for ((candidate_index = 0; candidate_index < ${#unique[@]}; candidate_index++)); do
      [[ "${unique[$candidate_index]}" = "$image" ]] && existing=1 && break
    done
    (( existing == 0 )) && unique+=("$image")
  done

  local archive_dir
  k3d_ensure_images "${unique[@]}" || return "$?"
  local nodes
  nodes=$(docker ps -a --filter "label=k3d.cluster=$cluster" \
    --format '{{.Label "k3d.role"}}' | awk '$0 == "server" || $0 == "agent" {n++} END {print n+0}') || return "$?"
  local eviction
  eviction=$(k3d_observed_eviction_threshold "$cluster") || {
    echo "cannot prove the running cluster's eviction threshold" >&2
    return 2
  }
  k3d_capacity_preflight "$nodes" \
    --eviction-threshold "$eviction" "${unique[@]}" || return "$?"
  archive_dir=$(mktemp -d)
  local archives=() status=0
  for image in "${unique[@]}"; do
    local archive="$archive_dir/$(k3d_archive_name "$image").tar"
    k3d_export_image "$image" "$archive" \
      || { status=$?; break; }
    archives+=("$archive")
  done
  if (( status == 0 )); then
    k3d image import "${archives[@]}" -c "$cluster" --mode direct >/dev/null || status=$?
  fi
  local cleanup_status=0
  rm -rf -- "$archive_dir" || cleanup_status=$?
  # The same canonical archive owner feeds K3D's direct stream importer.
  # There is no second images-volume store or transport-file cleanup path.
  (( status == 0 )) || return "$status"
  (( cleanup_status == 0 )) || return "$cleanup_status"
  kubectl --context "$context" -n kube-system delete pod -l k8s-app=kube-dns >/dev/null 2>&1 || true
  kubectl --context "$context" -n kube-system rollout status deploy/coredns --timeout=90s
}

# resolve_cargo_executable <package> <binary> [feature arguments...]
resolve_cargo_executable() {
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.96.0}" \
    node "$(dirname "${BASH_SOURCE[0]}")/../cargo_binary.mjs" "$@"
}

# Cause/effect table for the exact fault IO (not a fake Kubernetes authority):
# D1 complete original metadata -> one standard DELETE with exact UID/RV/body;
# D2 missing UID/RV or invalid cluster -> no kubectl; D3 conflict/transport failure
# -> original nonzero and exactly one call, never retry or claim closure.
k3d_delete_exact_pod_selftest() (
  local fixture
  fixture=$(mktemp -d)
  trap 'rm -f -- "$fixture/args" "$fixture/body" "$fixture/calls"; rmdir -- "$fixture"' EXIT
  local api_status=0
  run_with_deadline() { shift; "$@"; }
  kubectl() {
    printf '%s\0' "$@" >"$fixture/args"
    cat >"$fixture/body"
    printf 'call\n' >>"$fixture/calls"
    return "$api_status"
  }
  k3d_delete_exact_pod awaken-test exact source-pod original-uid original-rv
  python3 - "$fixture" "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" <<'PYTEST'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
assert root.joinpath('args').read_bytes().split(b'\0')[:-1] == [
    b'--context', b'k3d-awaken-test',
    f'--request-timeout={sys.argv[2]}s'.encode(), b'delete', b'--raw',
    b'/api/v1/namespaces/exact/pods/source-pod', b'-f', b'-']
assert json.loads(root.joinpath('body').read_bytes()) == {
    'apiVersion': 'v1', 'kind': 'DeleteOptions',
    'preconditions': {'uid': 'original-uid', 'resourceVersion': 'original-rv'}}
PYTEST
  [[ "$(wc -l <"$fixture/calls")" = 1 ]]
  ! k3d_delete_exact_pod awaken-test exact source-pod '' original-rv
  ! k3d_delete_exact_pod awaken-test exact source-pod original-uid ''
  ! k3d_delete_exact_pod --all exact source-pod original-uid original-rv
  [[ "$(wc -l <"$fixture/calls")" = 1 ]]
  for api_status in 41 42; do
    local result=0 before
    before=$(wc -l <"$fixture/calls")
    k3d_delete_exact_pod awaken-test exact source-pod original-uid original-rv || result=$?
    [[ "$result" = "$api_status" && "$(wc -l <"$fixture/calls")" = "$((before + 1))" ]]
  done
)

k3d_harness_selftest() (
  k3d_delete_exact_pod_selftest
  # One IO fixture records the actual volume-create labels and models Docker's
  # existing-name reuse. All ownership tables below reuse this backend; none
  # reconstruct the production ownership predicate or a second volume matcher.
  local volume_fixture_root volume_fixture_dir
  volume_fixture_root=$(mktemp -d)
  volume_fixture_dir="$volume_fixture_root/original"
  mkdir -p "$volume_fixture_dir"
  trap 'command rm -rf -- "$volume_fixture_root"' EXIT
  local volume_fixture_managed="" volume_fixture_missing_name="" volume_fixture_inspection_error=0
  local volume_fixture_mutations="" volume_fixture_remove_error=0 volume_fixture_creation_error=0 volume_fixture_empty_reply=0 volume_fixture_race_name=""
  volume_fixture() {
    local verb="$2" value name="" label fixture_owner="" fixture_run="" fixture_nonce="" count=0 record format=""
    shift 2
    case "$verb" in
      create)
        while (( $# > 0 )); do
          if [[ "$1" = --label ]]; then
            label="$2"; shift 2
            case "$label" in
              awaken.e2e.owner=*) fixture_owner=${label#*=} ;;
              awaken.e2e.run=*) fixture_run=${label#*=} ;;
              awaken.e2e.creation=*) fixture_nonce=${label#*=} ;;
            esac
          else name="$1"; shift; fi
        done
        if [[ -z "$name" ]]; then
          [[ ! -f "$volume_fixture_dir/counter" ]] || read -r count <"$volume_fixture_dir/counter"
          count=$((count + 1)); printf '%s\n' "$count" >"$volume_fixture_dir/counter"
          printf -v name '%064x' "$count"
        fi
        if [[ "$name" = "$volume_fixture_race_name" && ! -f "$volume_fixture_dir/$name" ]]; then
          printf '%s|fixture-created|%s|%s|foreign-request\n' "$name" "$fixture_owner" "$fixture_run" >"$volume_fixture_dir/$name"
        fi
        if [[ ! -f "$volume_fixture_dir/$name" ]]; then
          printf '%s|fixture-created|%s|%s|%s\n' "$name" "$fixture_owner" "$fixture_run" "$fixture_nonce" >"$volume_fixture_dir/$name"
        fi
        printf '%s\n' "$name" >"$volume_fixture_dir/.last-created"
        [[ "$volume_fixture_empty_reply" = 1 ]] || printf '%s\n' "$name"
        return "$volume_fixture_creation_error" ;;
      inspect)
        [[ "$1" = --format ]] || return 99
        format="$2"; name="${@: -1}"
        [[ -f "$volume_fixture_dir/$name" && "$name" != "$volume_fixture_missing_name" ]] || return 1
        record=$(cat "$volume_fixture_dir/$name")
        if [[ "$format" = '{{.Name}}|{{.CreatedAt}}' ]]; then
          IFS='|' read -r name value fixture_owner fixture_run fixture_nonce <<<"$record"
          printf '%s|%s\n' "$name" "$value"
        else printf '%s\n' "$record"; fi
        return "$volume_fixture_inspection_error" ;;
      ls)
        for value in "$@"; do
          if [[ "$value" = label=app=k3d ]]; then
            [[ -z "$volume_fixture_managed" ]] || printf '%s\n' "$volume_fixture_managed"
            return 0
          elif [[ "$value" = name=^* ]]; then name=${value#name=^}; name=${name%\$}; fi
        done
        [[ -n "$name" ]] || return 99
        if [[ -f "$volume_fixture_dir/$name" && "$name" != "$volume_fixture_missing_name" ]]; then printf '%s\n' "$name"; fi ;;
      rm)
        [[ "$volume_fixture_remove_error" = 0 ]] || return "$volume_fixture_remove_error"
        name="${@: -1}"
        [[ "$*" != *'--force'* && "$1" != -f ]] || return 99
        volume_fixture_mutations+=" remove:$name"
        command rm -f -- "$volume_fixture_dir/$name" ;;
      *) return 99 ;;
    esac
    return 0
  }
  python3 "$(dirname "${BASH_SOURCE[0]}")/capacity.py" --self-test
  python3 "$(dirname "${BASH_SOURCE[0]}")/test_image_archive.py"
  # Private-context decision table: P1 valid caller-owned path before create
  # -> KUBECONFIG points at an empty private file; P2 valid exact cluster after
  # create -> only its kubeconfig is installed; P3 invalid cluster, missing
  # file, nonempty existing file, or option-shaped path -> no k3d call, no
  # overwrite and no ambient context fallback.
  (
    local private_file calls=0
    private_file=$(mktemp)
    trap 'rm -f -- "$private_file"' EXIT
    k3d() {
      calls=$((calls + 1))
      [[ "$*" = 'kubeconfig get awaken-test' ]] || return 99
      printf 'cluster: awaken-test\n'
    }
    ! k3d_use_private_kubeconfig --all
    k3d_use_private_kubeconfig "$private_file"
    [[ "$KUBECONFIG" = "$private_file" && ! -s "$private_file" ]]
    ! k3d_bind_private_kubeconfig --all
    [[ "$calls" = 0 ]]
    k3d_bind_private_kubeconfig awaken-test
    [[ "$calls" = 1 && "$(cat "$private_file")" = 'cluster: awaken-test' ]]
    ! k3d_use_private_kubeconfig "$private_file"
    [[ "$(cat "$private_file")" = 'cluster: awaken-test' ]]
    rm -f -- "$private_file"
    ! k3d_bind_private_kubeconfig awaken-test
    [[ "$calls" = 1 ]]
  )
  # Static-volume fixture table V1 invalid name/count => no Docker/Kubernetes;
  # V2 existing directory => stop before formatting; V3 formatter failure =>
  # no node eligibility; V4 fresh owned Node => one formatter then eligibility.
  # Mocks replace infrastructure only; the actual admission/helper executes.
  (
    local scenario calls=0 eligible=0
    docker() {
      calls=$((calls+1))
      if [[ "$1" = exec && "$scenario" = existing ]]; then return 7; fi
      if [[ "$1" = run && "$scenario" = format-fails ]]; then return 8; fi
      return 0
    }
    kubectl() { eligible=$((eligible+1)); }
    calls=0; scenario=invalid
    ! k3d_prepare_local_storage --all 4
    ! k3d_prepare_local_storage awaken-test 0
    ! k3d_prepare_local_storage awaken-test 17
    [[ "$calls" = 0 && "$eligible" = 0 ]]
    scenario=existing
    ! k3d_prepare_local_storage awaken-test 4
    [[ "$calls" = 1 && "$eligible" = 0 ]]
    calls=0; scenario=format-fails
    ! k3d_prepare_local_storage awaken-test 4
    [[ "$calls" = 2 && "$eligible" = 0 ]]
    calls=0; scenario=fresh
    k3d_prepare_local_storage awaken-test 4
    [[ "$calls" = 2 && "$eligible" = 1 ]]
  )
  # Digest cause/effect table: C1 inspected image has real RepoDigests;
  # C2 Docker observation succeeds; C3 reference has an exact SHA-256 shape.
  # I1 C1+C2+C3 => immutable manifest reference, stable under alias order;
  # I2 !C1 => fail (config Id is never a fallback); I3 !C2 even with valid
  # stdout => fail; I4 !C3 => reject. No build/import occurs in this observer;
  # the live gate imports and executes the resulting image itself.
  (
    local document result=0
    local digest="registry.test/sandbox@sha256:$(printf 'a%.0s' {1..64})"
    docker() {
      [[ "$*" = 'image inspect --format {{json .RepoDigests}} -- exact:built' ]] || return 99
      printf '%s' "$document"
      return "$result"
    }
    # The shared deadline process cannot call a shell fixture function; its
    # process termination tests remain in the existing deadline owner.
    run_with_deadline() { shift; "$@"; }
    document="[\"$digest\",\"z-$digest\",\"$digest\"]"
    [[ "$(k3d_image_digest exact:built)" = "$digest" ]]
    result=7
    ! k3d_image_digest exact:built >/dev/null 2>&1
    result=0
    for document in '[]' 'null' '{}' 'invalid-json' '[null]' '["tag:latest"]' '["repo@sha256:short"]'; do
      ! k3d_image_digest exact:built >/dev/null 2>&1
    done
  )
  # Render-binding cause/effect table: C1 one deployment placeholder + C2 one
  # validated immutable image + C3 successful Kustomize render => the exact
  # image appears once and no marker remains. Missing/duplicate markers, a
  # mutable reference, option-shaped input, or a render failure must produce no
  # usable manifest; Kubernetes apply is deliberately outside this function.
  (
    local digest="registry.test/sandbox@sha256:$(printf 'b%.0s' {1..64})"
    local document result=0 rendered
    kubectl() {
      [[ "$*" = "kustomize fixture" ]] || return 99
      printf '%s\n' "$document"
      return "$result"
    }
    document='container_image = "__AWAKEN_SANDBOX_IMAGE__"'
    rendered=$(k3d_render_sandbox_image fixture "$digest")
    [[ "$rendered" = "container_image = \"$digest\"" ]]
    for document in 'container_image = "tag:latest"'; do
      ! k3d_render_sandbox_image fixture "$digest" >/dev/null 2>&1
    done
    document='first = "__AWAKEN_SANDBOX_IMAGE__" second = "__AWAKEN_SANDBOX_IMAGE__"'
    [[ "$(k3d_render_sandbox_image fixture "$digest" 2 | grep -oF "$digest" | wc -l)" = 2 ]]
    ! k3d_render_sandbox_image fixture "$digest" 1 >/dev/null 2>&1
    document='container_image = "__AWAKEN_SANDBOX_IMAGE__"'
    ! k3d_render_sandbox_image fixture tag:latest >/dev/null 2>&1
    ! k3d_render_sandbox_image --fixture "$digest" >/dev/null 2>&1
    ! k3d_render_sandbox_image >/dev/null 2>&1
    result=7
    ! k3d_render_sandbox_image fixture "$digest" >/dev/null 2>&1
  )
  # Cause/effect decision table:
  # H1 canonical name/count/quantity + absent coordinate -> one create with server and agent
  # thresholds; H2 zero agents -> no agent threshold; H3 option-shaped name or
  # non-numeric count/percentage -> rejected before any k3d effect; H4 duplicate
  # image coordinates -> one canonical single-platform archive per unique image
  # plus Pause/CoreDNS prerequisites, direct stream import and local archive
  # cleanup; H5 malformed port-forward identity ->
  # rejection before kubectl; H6 valid preferred port -> an available IPv4 port;
  # H7 malformed preferred port -> rejection before a socket probe; H8 an exact
  # Registry coordinate is attached once and malformed input is rejected before
  # cluster mutation; H9 tools available -> continue in optional and strict mode;
  # H10 tools unavailable + optional mode -> successful explicit skip; H11 tools
  # unavailable + strict mode -> failure; H12 malformed strictness -> configuration
  # failure; H13 no build-network override -> Docker's default build network;
  # H14 explicit override -> exactly one typed Docker build network option;
  # H15 each cluster-owned resource independently makes deletion fail until it
  # is absent; H16 already-absent retained resources remain idempotent, while
  # Docker failures keep their raw nonzero status; H17 an observation error is
  # never interpreted as absence; H18 cluster cleanup failure still removes
  # caller-owned local artifacts while preserving the failure; H19-H22 compose
  # body and cleanup status through a real EXIT trap as follows.
  #
  # | Rule | body status | exact cleanup | Effect |
  # | H19  | success     | residue       | cleanup failure |
  # | H20  | success     | absent        | success |
  # | H21  | failure     | absent        | original body failure |
  # | H22  | failure     | residue       | cleanup failure takes precedence |
  # The scenario tests own network and topology effects.
  # Storage-image decision table: S1 exact cluster + both observed image
  # coordinates + import succeeds => every Kubernetes read/write uses that
  # cluster's explicit context, then restart and wait; S2 either coordinate
  # absent => no import/restart; S3 import fails => no restart; S4 restart/wait
  # fails => propagate failure; S5 invalid cluster => zero Kubernetes/import IO.
  # The mock deliberately accepts no ambient-context call, so a developer's
  # unrelated current context cannot satisfy S1.
  # Calls use the actual shared function, not a reimplementation of its decisions.
  (
    local mode calls
    kubectl() {
      [[ "$1 $2" = "--context k3d-awaken-test" ]] || return 98
      shift 2
      case "$*" in
        *"get deploy local-path-provisioner"*) [[ "$mode" = missing-provisioner ]] || printf provisioner:exact ;;
        *"get configmap local-path-config"*) [[ "$mode" = missing-helper ]] || printf 'image: helper:exact\n' ;;
        *"rollout restart"*) calls+=" restart"; [[ "$mode" != restart-fails ]] ;;
        *"rollout status"*) calls+=" wait"; [[ "$mode" != wait-fails ]] ;;
        *) return 99 ;;
      esac
    }
    k3d_import_images() {
      [[ "$*" = "awaken-test role:exact provisioner:exact helper:exact" ]] || return 99
      calls+=" import"
      [[ "$mode" != import-fails ]]
    }
    for mode in success missing-provisioner missing-helper import-fails restart-fails wait-fails; do
      calls=""
      if [[ "$mode" = success ]]; then
        k3d_import_storage_images awaken-test role:exact
      else
        ! k3d_import_storage_images awaken-test role:exact
      fi
      case "$mode" in
        success|wait-fails) [[ "$calls" = " import restart wait" ]] ;;
        missing-*) [[ -z "$calls" ]] ;;
        import-fails) [[ "$calls" = " import" ]] ;;
        restart-fails) [[ "$calls" = " import restart" ]] ;;
      esac
    done
    mode=success calls=""
    ! k3d_import_storage_images --all role:exact
    [[ -z "$calls" ]]
  )
  # Session-selection decision table: Q1 one exact scope among foreign/absent
  # annotations => that Pod; Q2 zero/two exact matches => reject; Q3 the exact
  # Pod is deleting or lacks name/UID => reject; Q4 a different replacement name
  # with the same exact scope => select its live identity. Physical continuity
  # is separately asserted by the scenario's retained PVC UID and byte oracle.
  # Q5 an API error remains failure even if the observer emitted valid JSON.
  (
    local document pod_key observation_status=0
    kubectl() {
      [[ "$*" = "-n ns get pods -l app=awaken-sandbox -o json" ]] || return 99
      printf '%s' "$document"
      return "$observation_status"
    }
    for pod_key in original replacement; do
      document="$(python3 - "$pod_key" <<'FIXTURE'
import json, sys
print(json.dumps({"items": [{"metadata": {"name": "decoy", "uid": "foreign", "annotations": {"awaken.dev/sandbox-scope": "foreign"}}}, {"metadata": {"name": sys.argv[1], "uid": "exact", "annotations": {"awaken.dev/sandbox-scope": "session-exact"}}}, {"metadata": {"name": "unannotated"}}]}))
FIXTURE
)"
      [[ "$(k3d_sandbox_pod_for_session ns session-exact)" = "$pod_key" ]]
      ! k3d_sandbox_pod_for_session ns absent >/dev/null 2>&1
      ! k3d_sandbox_pod_for_session ns "" >/dev/null 2>&1
      observation_status=70
      ! k3d_sandbox_pod_for_session ns session-exact >/dev/null 2>&1
      observation_status=0
      for fault in duplicate deleting missing-name missing-uid; do
        (
          document="$(python3 -c '
import json, sys
value = json.loads(sys.argv[1]); exact = value["items"][1]
fault = sys.argv[2]
if fault == "duplicate": value["items"].append(exact)
elif fault == "deleting": exact["metadata"]["deletionTimestamp"] = "2026-09-07T00:00:00Z"
else: exact["metadata"].pop(fault.removeprefix("missing-"))
print(json.dumps(value))
' "$document" "$fault")"
          ! k3d_sandbox_pod_for_session ns session-exact >/dev/null 2>&1
        )
      done
    done
  )
  # Cleanup ownership C/E table (actual helpers with IO fixtures, not source
  # string checks): O1 no creation receipt or existing/unknown coordinate =>
  # zero mutation; O2 successful creation freezes owner/run, actual CIDs,
  # network ID and images-volume incarnation; O3 foreign/unlabelled/replaced
  # node, extra network member or foreign volume borrower => retain the WHOLE
  # group; O4 failed inspection remains its raw nonzero even with valid stdout;
  # O5 network/volume incarnation drift => retain; O6 exact receipt => one
  # canonical teardown; O7 failed create with complete owned receipt can clean;
  # O8 empty/unconfirmed partial creation stays Unknown and is never name-deleted.
  (
    local volume_fixture_dir="$volume_fixture_root/ownership"
    mkdir -p "$volume_fixture_dir"
    local probe_cluster=awaken-owned node_present=0 network_present=0
    local original_id="$(printf 'c%.0s' {1..64})" probe_id
    local original_network="$(printf 'd%.0s' {1..64})" network_id
    probe_id="$original_id"; network_id="$original_network"
    local foreign_id="$(printf 'e%.0s' {1..64})" node_owner=fixture node_run=fixture-run
    local images_created=original extra_network="" extra_borrower=""
    local probe_volume="$(printf 'f%.0s' {1..64})" volume_present=0 probe_external=false volume_fixture_managed=""
    local inspection_error=0 create_error=0 empty_create=0 no_network_create=0 mutations="" status observer_error=0
    local registered_attached=0 disconnect_error=0 disconnect_effect=1 network_remove_error=0
    local unknown_id="$(printf 'a%.0s' {1..64})"
    K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
    export AWAKEN_E2E_RUN_ID=fixture-run
    e2e_docker_init fixture
    k3d_capacity_preflight() { return 0; }
    k3d() {
      case "$1 $2" in
        'cluster list')
          [[ "$observer_error" = 0 ]] || return "$observer_error"
          [[ "$node_present" = 0 ]] || printf '%s 1/1 0/0 true\n' "$probe_cluster" ;;
        'cluster create')
          [[ "$*" = *'--runtime-label awaken.e2e.owner=fixture@all'* \
            && "$*" = *'--runtime-label awaken.e2e.run=fixture-run@all'* ]] || return 99
          mutations+=" create"
          [[ "$no_network_create" = 0 ]] || return "$create_error"
          network_present=1; volume_present=1
          registered_attached=0
          [[ "$*" != *'--registry-use '* ]] || registered_attached=1
          if [[ "$empty_create" != 1 ]]; then
            node_present=1
            printf '%s|fixture-created|||\n' "$probe_volume" >"$volume_fixture_dir/$probe_volume"
          fi
          return "$create_error" ;;
        'cluster delete') return 99 ;;
        *) return 99 ;;
      esac
      return 0
    }
    docker() {
      if [[ "$1" = volume ]]; then volume_fixture "$@"; return "$?"; fi
      case "$1 $2" in
        'rm -f')
          [[ "${@: -1}" = "$probe_id" ]] || return 99
          mutations+=" cid-rm"; node_present=0
          [[ -n "$extra_borrower" ]] || command rm -f -- "$volume_fixture_dir/$probe_volume" ;;
        'ps -aq')
          if [[ "$*" = *'volume='* ]]; then
            [[ "$node_present" = 0 ]] || printf '%s\n' "$probe_id"
            [[ -z "$extra_borrower" ]] || printf '%s\n' "$extra_borrower"
          else [[ "$node_present" = 0 ]] || printf '%s\n' "$probe_id"; fi ;;
        'inspect --format')
          if [[ "${@: -1}" = k3d-fixture-registry.localhost ]]; then printf '%s\n' "$foreign_id"
          elif [[ "$*" = *'{{.Id}}|{{index '* ]]; then
            printf '%s|%s|%s|%s|%s|%s|%s\n' "$probe_id" "$node_owner" "$node_run" "$probe_cluster" "$network_id" "$probe_external" "$K3D_CREATED_IMAGE_VOLUME"
          elif [[ "$*" = *'{{.Id}}'* ]]; then printf '%s\n' "$probe_id"
          else printf '%s\n' "$K3D_CREATED_IMAGE_VOLUME" "$probe_volume"; fi
          return "$inspection_error" ;;
        'network inspect')
          if [[ "$*" = *'{{.Id}}|{{.Name}}'* ]]; then printf '%s|k3d-%s\n' "$network_id" "$probe_cluster"
          elif [[ "$*" = *'{{.Id}}'* ]]; then printf '%s\n' "$network_id"
          else
            [[ "$node_present" = 0 ]] || printf '%s\n' "$probe_id"
            [[ "$registered_attached" = 0 ]] || printf '%s\n' "$foreign_id"
            [[ -z "$extra_network" ]] || printf '%s\n' "$extra_network"
          fi ;;
        'network disconnect')
          [[ "$3/$4" = "$network_id/$foreign_id" ]] || return 99
          mutations+=" registry-disconnect"
          [[ "$disconnect_effect" = 0 ]] || registered_attached=0
          return "$disconnect_error" ;;
        'network rm')
          if [[ "$network_remove_error" != 0 ]]; then extra_network="$unknown_id"; return "$network_remove_error"; fi
          mutations+=" network-rm"; network_present=0 ;;
        'network ls')
          if [[ "$network_present" = 1 ]]; then
            if [[ "$*" = *'{{.ID}}'* ]]; then printf '%s\n' "$network_id"
            else printf 'k3d-%s\n' "$probe_cluster"; fi
          fi ;;
        'ps -a') [[ "$node_present" = 0 ]] || printf 'k3d-%s-server-0\n' "$probe_cluster" ;;
        *) return 99 ;;
      esac
      return 0
    }
    kubectl() { [[ "$*" = 'config get-contexts --no-headers' ]]; }
    node_present=1
    k3d_delete_cluster "$probe_cluster"
    [[ -z "$mutations" && "$node_present" = 1 ]]
    status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
    [[ "$status" = 2 && -z "$mutations" && "$node_present" = 1 ]]
    node_present=0; observer_error=37
    status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
    [[ "$status" = 2 && -z "$mutations" ]]
    observer_error=0
    k3d_create_cluster "$probe_cluster" 0
    [[ "$K3D_CREATION_CONFIRMED" = 1 && "${K3D_CREATED_CONTAINER_IDS[0]}" = "$original_id" ]]
    [[ "$(_k3d_created_node_id "k3d-$probe_cluster-server-0")" = "$original_id" ]]
    local bad_node
    for bad_node in --all k3d-other-server-0 "k3d-$probe_cluster-serverlb"; do
      status=0; _k3d_created_node_id "$bad_node" || status=$?
      [[ "$status" = 2 && "$mutations" = ' create' ]]
    done
    probe_id="$foreign_id"
    status=0; _k3d_created_node_id "k3d-$probe_cluster-server-0" || status=$?
    [[ "$status" = 2 ]]
    probe_id="$original_id"
    local fault original_volume_record
    original_volume_record=$(cat "$volume_fixture_dir/$probe_volume")
    for fault in foreign unlabelled replacement network-member volume-borrower inspection network-id volume-incarnation managed-volume reused-network; do
      node_owner=fixture; node_run=fixture-run; probe_id="$original_id"; network_id="$original_network"
      extra_network=""; extra_borrower=""; inspection_error=0; images_created=original; volume_fixture_managed=""; probe_external=false
      printf '%s\n' "$original_volume_record" >"$volume_fixture_dir/$probe_volume"
      case "$fault" in
        foreign) node_run=other-run ;;
        unlabelled) node_owner='<no value>'; node_run='<no value>' ;;
        replacement) probe_id="$foreign_id" ;;
        network-member) extra_network="$foreign_id" ;;
        volume-borrower) extra_borrower="$foreign_id" ;;
        inspection) inspection_error=37 ;;
        network-id) network_id="$foreign_id" ;;
        volume-incarnation) printf '%s|replacement|||\n' "$probe_volume" >"$volume_fixture_dir/$probe_volume" ;;
        managed-volume) volume_fixture_managed=foreign-managed ;;
        reused-network) probe_external=true ;;
      esac
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      if [[ "$fault" = inspection ]]; then
        [[ "$status" = 37 ]] || { printf 'ownership rule %s returned %s, expected 37\n' "$fault" "$status" >&2; return 1; }
      else
        [[ "$status" = 2 ]] || { printf 'ownership rule %s returned %s, expected 2\n' "$fault" "$status" >&2; return 1; }
      fi
      [[ "$mutations" = ' create' && "$node_present" = 1 && "$network_present" = 1 ]]
    done
    node_owner=fixture; node_run=fixture-run; probe_id="$original_id"; network_id="$original_network"
    extra_network=""; extra_borrower=""; inspection_error=0; images_created=original; volume_fixture_managed=""; probe_external=false
    printf '%s\n' "$original_volume_record" >"$volume_fixture_dir/$probe_volume"
    node_present=0; extra_borrower="$foreign_id"
    status=0; k3d_delete_cluster "$probe_cluster" || status=$?
    [[ "$status" = 2 && "$mutations" = ' create' && "$network_present" = 1 && "$volume_present" = 1 ]]
    extra_borrower=""
    k3d_delete_cluster "$probe_cluster"
    [[ "$mutations" = ' create network-rm' && -z "$K3D_CREATED_CLUSTER" ]]
    create_error=41
    status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
    [[ "$status" = 41 && "$K3D_CREATION_CONFIRMED" = 1 ]]
    k3d_delete_cluster "$probe_cluster"
    empty_create=1
    status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
    [[ "$status" = 41 && "$K3D_CREATION_CONFIRMED" = 0 && "$network_present" = 1 ]]
    local before_mutations="$mutations"
    status=0; k3d_delete_cluster "$probe_cluster" || status=$?
    [[ "$status" = 2 && "$mutations" = "$before_mutations" && "$network_present" = 1 ]]
    # The image volume was already selected/claimed before K3D failed. Its
    # exact creation fact remains visible; Unknown is not an empty cleanup.
    local held_image
    held_image=$(_e2e_docker_owned_volume "$K3D_CREATED_IMAGE_VOLUME")
    [[ -n "$held_image" && -f "$volume_fixture_dir/$K3D_CREATED_IMAGE_VOLUME" ]]
    # O8b failure can occur even before the network exists: preserve the
    # first K3D failure AND the previously created image receipt; refuse to
    # turn that known allocation/unknown group into cleanup success.
    (
      local volume_fixture_dir="$volume_fixture_root/no-node-or-network"
      mkdir -p "$volume_fixture_dir"
      K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
      node_present=0; network_present=0; mutations=""; create_error=41; no_network_create=1
      status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
      [[ "$status" = 41 && "$node_present" = 0 && "$network_present" = 0 ]]
      held_image=$(_e2e_docker_owned_volume "$K3D_CREATED_IMAGE_VOLUME")
      [[ -n "$held_image" ]]
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 2 && "$(_e2e_docker_owned_volume "$K3D_CREATED_IMAGE_VOLUME")" = "$held_image" ]]
    )
    # O11/O12 model the two after-preflight creation windows at their actual
    # backend boundaries: K3D reports network reuse; Docker returns another
    # request's existing named image volume. Neither post-query identity is
    # creation authority. Unknown/mixed groups are never name-cleaned.
    (
      local volume_fixture_dir="$volume_fixture_root/network-window"
      mkdir -p "$volume_fixture_dir"
      K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
      node_present=0; network_present=0; mutations=""; create_error=0; empty_create=0
      extra_network=""; extra_borrower=""; probe_external=true
      status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
      [[ "$status" = 2 && "$K3D_CREATION_CONFIRMED" = 0 && "$node_present" = 1 && "$network_present" = 1 ]]
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 2 && "$mutations" = ' create' ]]
    )
    (
      local volume_fixture_dir="$volume_fixture_root/image-window"
      mkdir -p "$volume_fixture_dir"
      K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
      node_present=0; network_present=0; mutations=""; create_error=0; empty_create=0
      extra_network=""; extra_borrower=""; probe_external=false
      volume_fixture_race_name="k3d-$probe_cluster-images"
      status=0; k3d_create_cluster "$probe_cluster" 0 || status=$?
      [[ "$status" = 2 && -z "$K3D_CREATED_CLUSTER" && "$node_present" = 0 && -z "$mutations" ]]
      [[ -f "$volume_fixture_dir/$volume_fixture_race_name" && -z "$E2E_CREATED_VOLUME" ]]
      k3d_delete_cluster "$probe_cluster"
      [[ -f "$volume_fixture_dir/$volume_fixture_race_name" && -z "$mutations" ]]
    )
    # O9 the selected pre-existing Registry is a retained attachment only.
    # An extra member blocks BEFORE disconnect; a failed/lost disconnect ACK
    # or unchanged post-state blocks before any node deletion; exact detach
    # permits cleanup but never removes that external Registry container.
    (
      local volume_fixture_dir="$volume_fixture_root/registry"
      mkdir -p "$volume_fixture_dir"
      K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
      node_present=0; network_present=0; mutations=""; create_error=0; empty_create=0
      extra_network=""; extra_borrower=""; registered_attached=0
      k3d_create_cluster "$probe_cluster" 0 128Mi k3d-fixture-registry.localhost:5000
      [[ "$K3D_CREATED_REGISTRY_ID" = "$foreign_id" && "$registered_attached" = 1 ]]
      extra_network="$unknown_id"
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 2 && "$mutations" = ' create' && "$registered_attached" = 1 ]]
      extra_network=""; disconnect_error=37; disconnect_effect=0
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 37 && "$node_present" = 1 && "$registered_attached" = 1 ]]
      disconnect_effect=1
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 37 && "$node_present" = 1 && "$registered_attached" = 0 ]]
      # Restore only the fixture's originally selected attachment for the
      # independent successful-ACK/no-effect row; production never reconnects.
      registered_attached=1; disconnect_effect=0; disconnect_error=0
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 2 && "$node_present" = 1 && "$registered_attached" = 1 ]]
      disconnect_effect=1
      k3d_delete_cluster "$probe_cluster"
      [[ "$node_present" = 0 && "$network_present" = 0 && "$registered_attached" = 0 ]]
      [[ "$mutations" = *' registry-disconnect cid-rm network-rm' ]]
    )
    # O10 a late endpoint makes the real non-force network operation fail.
    # The exact-node effects may already be complete, but no foreign endpoint
    # is removed or reported closed. Only fixture-owned removal enables retry.
    (
      local volume_fixture_dir="$volume_fixture_root/late-endpoint"
      mkdir -p "$volume_fixture_dir"
      K3D_CREATED_CLUSTER=""; K3D_CREATION_CONFIRMED=0
      node_present=0; network_present=0; mutations=""; create_error=0; empty_create=0
      extra_network=""; extra_borrower=""; registered_attached=0
      k3d_create_cluster "$probe_cluster" 0
      network_remove_error=37
      status=0; k3d_delete_cluster "$probe_cluster" || status=$?
      [[ "$status" = 37 && "$node_present" = 0 && "$network_present" = 1 && "$extra_network" = "$unknown_id" ]]
      network_remove_error=0; extra_network=""
      k3d_delete_cluster "$probe_cluster"
      [[ "$network_present" = 0 ]]
    )
  )
  # Explicit-volume C/E table: V1 a fresh generated name and matching checked
  # identity => retained receipt; V2 absent owned volume => idempotent success;
  # V3 foreign/unlabelled/recreated or unrecorded identity => zero deletion;
  # V4 any own/foreign/mixed mount => keep, never delete its containers;
  # V5 observation error with valid stdout => raw error, never "absent";
  # V6 matching identity and no mounts => exactly one volume removal.
  (
    local volume_fixture_dir="$volume_fixture_root/explicit"
    mkdir -p "$volume_fixture_dir"
    local probe_volume original_record status
    E2E_VOLUME_RECEIPTS=()
    volume_fixture_mutations=""
    export AWAKEN_E2E_RUN_ID=fixture-run
    e2e_docker_init fixture
    docker() {
      if [[ "$1" = volume ]]; then volume_fixture "$@"; return "$?"; fi
      [[ "$1 $2" = 'ps -aq' ]] || return 99
      [[ -z "$probe_mounts" ]] || printf '%s\n' "$probe_mounts"
    }
    local probe_mounts=""
    e2e_docker_create_volume
    probe_volume="$E2E_CREATED_VOLUME"
    original_record=$(cat "$volume_fixture_dir/$probe_volume")
    [[ "${#E2E_VOLUME_RECEIPTS[@]}" = 1 ]]
    local fault
    for fault in other unlabelled recreated own-mount mixed-mount io; do
      printf '%s\n' "$original_record" >"$volume_fixture_dir/$probe_volume"
      probe_mounts=""; volume_fixture_inspection_error=0
      case "$fault" in
        other) printf '%s\n' "${original_record/|fixture-run|/|other-run|}" >"$volume_fixture_dir/$probe_volume" ;;
        unlabelled) printf '%s\n' "${original_record/|fixture|/||}" >"$volume_fixture_dir/$probe_volume" ;;
        recreated) printf '%s\n' "${original_record/|fixture-created|/|replacement|}" >"$volume_fixture_dir/$probe_volume" ;;
        own-mount) probe_mounts=own-cid ;;
        mixed-mount) probe_mounts=$'own-cid\nforeign-or-unlabelled-cid' ;;
        io) volume_fixture_inspection_error=37 ;;
      esac
      status=0; e2e_docker_rm_volumes "$probe_volume" || status=$?
      if [[ "$fault" = io ]]; then [[ "$status" = 37 ]]; else [[ "$status" = 2 ]]; fi
      [[ -z "$volume_fixture_mutations" && -f "$volume_fixture_dir/$probe_volume" ]]
    done
    printf '%s\n' "$original_record" >"$volume_fixture_dir/$probe_volume"
    probe_mounts=""; volume_fixture_inspection_error=0
    status=0; e2e_docker_rm_volumes unrecorded || status=$?
    [[ "$status" = 2 && -z "$volume_fixture_mutations" ]]
    # V8 a borrower can arrive after the read: the removal boundary reports
    # failure and the volume remains. Never normalize that refusal to success.
    volume_fixture_remove_error=37
    status=0; e2e_docker_rm_volumes "$probe_volume" || status=$?
    [[ "$status" = 37 && -f "$volume_fixture_dir/$probe_volume" && -z "$volume_fixture_mutations" ]]
    volume_fixture_remove_error=0
    e2e_docker_rm_volumes "$probe_volume"
    [[ "$volume_fixture_mutations" = " remove:$probe_volume" && ! -f "$volume_fixture_dir/$probe_volume" ]]
    e2e_docker_rm_volumes "$probe_volume"
    [[ "$volume_fixture_mutations" = " remove:$probe_volume" ]]
    # V7 Docker returns an existing named volume unchanged: a NEW request
    # nonce must reject it, even with identical owner/run and coarse timestamps.
    e2e_docker_create_volume --name k3d-fixture-images
    original_record=$(cat "$volume_fixture_dir/k3d-fixture-images")
    status=0; e2e_docker_create_volume --name k3d-fixture-images || status=$?
    [[ "$status" = 2 && -z "$E2E_CREATED_VOLUME" ]]
    [[ "$(cat "$volume_fixture_dir/k3d-fixture-images")" = "$original_record" ]]
    e2e_docker_rm_volumes k3d-fixture-images
    e2e_docker_create_volume --name k3d-fixture-images
    [[ "$E2E_CREATED_VOLUME" = k3d-fixture-images && "${#E2E_VOLUME_RECEIPTS[@]}" = 2 ]]
    [[ "$(cat "$volume_fixture_dir/k3d-fixture-images")" != "$original_record" ]]
    e2e_docker_rm_volumes k3d-fixture-images
    # V9 partial create reply / successful create plus failed inspection:
    # preserve raw 37, keep the pending fact, grant no receipt, and refuse a
    # false successful empty cleanup. Only this fake daemon's explicit recovery
    # below removes its known data; production has no late-install bypass.
    local failure_phase before_receipts pending
    for failure_phase in create-reply empty-create-reply inspection; do
      before_receipts="${#E2E_VOLUME_RECEIPTS[@]}"
      volume_fixture_creation_error=0; volume_fixture_inspection_error=0; volume_fixture_empty_reply=0
      if [[ "$failure_phase" = inspection ]]; then volume_fixture_inspection_error=37
      else
        volume_fixture_creation_error=37
        [[ "$failure_phase" != empty-create-reply ]] || volume_fixture_empty_reply=1
      fi
      status=0; e2e_docker_create_volume || status=$?
      pending="$E2E_UNCONFIRMED_VOLUME"
      [[ "$status" = 37 && -n "$pending" && -z "$E2E_CREATED_VOLUME" \
        && "${#E2E_VOLUME_RECEIPTS[@]}" = "$before_receipts" ]]
      status=0; k3d_delete_cluster awaken-unowned || status=$?
      [[ "$status" = 2 ]]
      if [[ "$failure_phase" = empty-create-reply ]]; then pending=$(cat "$volume_fixture_dir/.last-created"); fi
      [[ -f "$volume_fixture_dir/$pending" ]]
      command rm -f -- "$volume_fixture_dir/$pending"
      E2E_UNCONFIRMED_VOLUME=""
    done
  )
  local k3d_calls=() docker_saves=() docker_execs=() docker_builds=() deadline_calls=()
  local observation_error="" residue="" import_failure=0 archive_cleanup_failure=0
  local image_observation_status=0 image_pull_status=0 image_presence_calls=()
  local capacity_failure=0 capacity_calls=() eviction_observation_fault=""
  local fixture_cluster="" fixture_network_cluster="" fixture_shared=0
  local fixture_cid="$(printf 'a%.0s' {1..64})" fixture_network="$(printf 'b%.0s' {1..64})"
  local fixture_volume="$(printf '0%.0s' {1..63})1"
  k3d_capacity_preflight() {
    capacity_calls+=("$*")
    return "$capacity_failure"
  }
  k3d() {
    k3d_calls+=("$*")
    if [[ "$1 $2" = "image import" ]]; then return "$import_failure"; fi
    if [[ "$1 $2" = "cluster create" ]]; then
      fixture_cluster="$3"; fixture_network_cluster="$3"; fixture_shared=0
      [[ "$*" != *'--volume '* ]] || fixture_shared=1
    elif [[ "$1 $2" = "cluster delete" ]]; then
      return 99
    elif [[ "$1 $2" = "cluster list" ]]; then
      [[ "$observation_error" != "k3d" ]] || return 1
      [[ -z "$fixture_cluster" ]] || printf '%s 1/1 0/0 true\n' "$fixture_cluster"
      [[ "$residue" != "cluster" ]] || printf '%s\n' 'awaken-test 1/1 0/0 true'
    fi
    return 0
  }
  docker() {
    if [[ "$1" = volume ]]; then
      [[ "$observation_error" != docker ]] || return 1
      if [[ "$2" = ls && "$residue" = volume && "$*" = *'name=^k3d-awaken-test-images$'* ]]; then
        printf '%s\n' k3d-awaken-test-images
        return 0
      fi
      volume_fixture "$@"
      return "$?"
    elif [[ "$1 $2" = "network inspect" ]]; then
      if [[ "$*" = *'{{.Id}}|{{.Name}}'* ]]; then printf '%s|k3d-%s\n' "$fixture_network" "$fixture_network_cluster"
      elif [[ "$*" = *'{{.Id}}'* ]]; then printf '%s\n' "$fixture_network"
      else [[ -z "$fixture_cluster" ]] || printf '%s\n' "$fixture_cid"; fi
    elif [[ "$1 $2" = "network ls" ]]; then
      [[ "$observation_error" != "docker" ]] || return 1
      if [[ "$*" = *'{{.ID}}'* ]]; then
        [[ -z "$fixture_network_cluster" && "$residue" != network ]] || printf '%s\n' "$fixture_network"
      elif [[ -n "$fixture_network_cluster" ]]; then printf 'k3d-%s\n' "$fixture_network_cluster"
      else [[ "$residue" != "network" ]] || printf '%s\n' 'k3d-awaken-test'; fi
    elif [[ "$1 $2" = "network rm" ]]; then
      [[ "${@: -1}" = "$fixture_network" ]] || return 99
      fixture_network_cluster=""
    elif [[ "$1 $2" = 'rm -f' ]]; then
      [[ "${@: -1}" = "$fixture_cid" ]] || return 99
      fixture_cluster=""
    elif [[ "$1" = "inspect" ]]; then
      if [[ "$*" = *'{{.Id}} -- k3d-awaken-registry.localhost' ]]; then
        printf '%064d\n' 2
      elif [[ "$*" = *'{{.Id}}|{{index '* ]]; then
        printf '%s|%s|%s|%s|%s|false|%s\n' "$fixture_cid" "$E2E_OWNER" "$E2E_RUN_ID" "$fixture_cluster" "$fixture_network" "$K3D_CREATED_IMAGE_VOLUME"
      elif [[ "$*" = *'{{range .Mounts}}'* ]]; then
        printf '%s\n' "$K3D_CREATED_IMAGE_VOLUME"
        [[ "$fixture_shared" = 0 ]] || printf '%s\n' "$fixture_volume"
      elif [[ "$*" = *'{{.Id}}'* ]]; then printf '%s\n' "$fixture_cid"
      fi
    elif [[ "$1" = "ps" ]]; then
      [[ "$observation_error" != "docker" ]] || return 1
      if [[ "$*" = *'label=k3d.cluster='* || "$*" = *'volume='* || "$*" = *'id='* ]]; then
        [[ -z "$fixture_cluster" ]] || printf '%s\n' "$fixture_cid"
      elif [[ -n "$fixture_cluster" ]]; then printf 'k3d-%s-server-0\n' "$fixture_cluster"
      else [[ "$residue" != "container" ]] || printf '%s\n' 'k3d-awaken-test-server-0'; fi
    fi
    return 0
  }

  kubectl() {
    if [[ "$1 $2" = "config get-contexts" ]]; then
      [[ "$observation_error" != "kubectl" ]] || return 1
      [[ "$residue" != "context" ]] || printf '%s\n' '  k3d-awaken-test k3d-awaken-test admin@k3d-awaken-test'
    fi
  }

  # Diagnostic/build deadline decision table: B1 a valid build deadline runs
  # the exact Docker command once; B2 malformed/zero deadlines are rejected
  # before Docker; D1 a valid cluster and diagnostic deadline bind kubectl to
  # that exact context and request timeout; D2 invalid cluster/deadline issues
  # no kubectl call. The shared deadline helper owns descendant termination.
  run_with_deadline() {
    local seconds="$1"
    shift
    deadline_calls+=("$seconds:$*")
    "$@"
  }
  k3d_diagnostic_kubectl awaken-test -n exact get pods
  [[ "${deadline_calls[0]}" = "${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}:kubectl --context k3d-awaken-test --request-timeout=${K3D_DIAGNOSTIC_TIMEOUT_SECONDS}s -n exact get pods" ]]
  local saved_diagnostic_timeout="$K3D_DIAGNOSTIC_TIMEOUT_SECONDS"
  K3D_DIAGNOSTIC_TIMEOUT_SECONDS=0
  ! k3d_diagnostic_kubectl awaken-test get pods
  K3D_DIAGNOSTIC_TIMEOUT_SECONDS="$saved_diagnostic_timeout"
  ! k3d_diagnostic_kubectl --all get pods

  k3d_validate_name "awaken-test-1"
  ! k3d_validate_name "--all"
  # CRI decision table: R1 exact running task + one KILL + same exited/137
  # -> success; R2 lost ACK with R1 receipt -> same success; R3 running, other
  # id, zero exit or missing evidence -> fail, without a second signal; R4
  # malformed/foreign/not-running target -> no signal. R5 delayed exit -> join
  # the same task. Real Kubernetes phases additionally join Pod UID + CRI id.
  (
    local container="$(printf 'd%.0s' {1..64})" initial=CONTAINER_RUNNING
    local final=CONTAINER_EXITED exit_code=137 ack=0 kills=0 waits=0 delayed=0
    local observed_id="$container" provider_error=0 calls=0 result
    k3d_validate_node_target() { [[ "$1/$2" = awaken-test/k3d-awaken-test-agent-0 ]]; }
    timeout() { shift; "$@"; }
    sleep() { waits=$((waits+1)); }
    docker() {
      [[ "$1 $2" = 'exec k3d-awaken-test-agent-0' ]] || return 98
      if [[ "$3" = ctr ]]; then
        [[ "$*" = *"tasks kill --signal SIGKILL --all $container" ]] || return 98
        kills=$((kills+1)); return "$ack"
      fi
      [[ "$3 $4 $5" = "crictl inspect $container" ]] || return 98
      ((provider_error == 0)) || return "$provider_error"
      local state="$initial" id="$container"
      if ((kills > 0)); then
        state="$final"; id="$observed_id"
        if ((waits < delayed)); then state=CONTAINER_RUNNING; fi
      fi
      printf '{"status":{"id":"%s","state":"%s","exitCode":%s}}\n' "$id" "$state" "$exit_code"
    }
    for ack in 0 41; do
      kills=0; k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container"
      [[ "$kills" = 1 ]]
    done
    ack=0; final=CONTAINER_RUNNING; kills=0
    ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container" 2>/dev/null
    [[ "$kills" = 1 ]]
    final=CONTAINER_EXITED
    for exit_code in 0 1; do
      kills=0; ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container" 2>/dev/null
      [[ "$kills" = 1 ]]
    done
    exit_code=137; observed_id="$(printf 'e%.0s' {1..64})"; kills=0
    ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container" 2>/dev/null
    [[ "$kills" = 1 ]]
    observed_id="$container"; initial=CONTAINER_EXITED; kills=0
    ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container"
    [[ "$kills" = 0 ]]
    initial=CONTAINER_RUNNING; provider_error=42
    ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container" 2>/dev/null
    [[ "$kills" = 0 ]]
    provider_error=0
    ! k3d_hard_stop_cri_container awaken-test k3d-other-agent-0 "$container"
    ! k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 bad
    [[ "$kills" = 0 ]]
    delayed=3; waits=0
    k3d_hard_stop_cri_container awaken-test k3d-awaken-test-agent-0 "$container"
    [[ "$kills" = 1 && "$waits" = 3 ]]
  )

  # Hard-stop decision table: P1 exact running node + one SIGKILL request +
  # false/137 observation => physical termination even if the daemon lost its
  # exit-event ACK; P2 malformed/foreign/replaced/already-stopped => no signal;
  # P3 inspect failure => no proof, preserve error; P4 still-running/wrong exit
  # => no proof, preserve a failed command status when present. P1 x ACK 0/41
  # and P4 x ACK 0/41 cover intent/result independence, with one signal per row.
  # P5 running observations followed by false/137 within the read budget => P1;
  # P6 delayed observer error => no proof. The clock/transport doubles below
  # skip real sleeps but preserve the exact attempt and one-signal inventory.
  # A named kill or graceful stop is rejected. Only P1 permits Pod replacement.
  (
    local container="$(printf 'c%.0s' {1..64})" owner=awaken-test
    local running=true identity="$container" stopped='false|137'
    local kill_error=0 observe_error=0 target_error=0 identity_error=0 kills=0 result
    local waits=0 stopped_after=0 error_after=0
    sleep() {
      [[ "$1" = 0.2 ]] || return 98
      waits=$((waits + 1))
      [[ "$waits" != "$stopped_after" ]] || stopped='false|137'
      [[ "$waits" != "$error_after" ]] || observe_error=42
      return 0
    }
    timeout() {
      [[ "$1" =~ ^[1-9][0-9]*s$ ]] || return 98
      shift
      "$@"
    }
    docker() {
      case "$*" in
        'inspect --format '*'.Config.Labels'*' -- k3d-awaken-test-agent-0')
          [[ "$target_error" = 0 ]] || return "$target_error"
          printf '%s|agent\n' "$owner" ;;
        'inspect --format {{.Id}}|{{.State.Running}} -- k3d-awaken-test-agent-0')
          [[ "$identity_error" = 0 ]] || return "$identity_error"
          printf '%s|%s\n' "$identity" "$running" ;;
        "kill --signal KILL $container") kills=$((kills + 1)); return "$kill_error" ;;
        "inspect --format {{.State.Running}}|{{.State.ExitCode}} -- $container")
          [[ "$observe_error" = 0 ]] || return "$observe_error"
          printf '%s\n' "$stopped" ;;
        *) return 99 ;;
      esac
    }
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container"
    [[ "$kills" = 1 ]]
    for invalid in '' --all k3d-other-agent-0; do
      ! k3d_hard_stop_node awaken-test "$invalid" "$container" 2>/dev/null
    done
    ! k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 malformed
    owner=foreign
    ! k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container"
    owner=awaken-test
    identity="$(printf 'd%.0s' {1..64})"
    ! k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container"
    identity="$container"; running=false
    ! k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container"
    [[ "$kills" = 1 ]]
    running=true; target_error=39; result=0
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
    [[ "$result" = 39 && "$kills" = 1 ]]
    target_error=0; identity_error=40; result=0
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
    [[ "$result" = 40 && "$kills" = 1 ]]
    identity_error=0; kill_error=41; result=0
    local before_kills=$kills expected_failure
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
    [[ "$result" = 0 && "$kills" = $((before_kills + 1)) ]]
    kill_error=0; observe_error=42; result=0
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
    [[ "$result" = 42 ]]
    observe_error=0
    for kill_error in 0 41; do
      stopped_after=0
      for stopped in 'true|0' 'false|0' 'false|143'; do
        waits=0; result=0; before_kills=$kills; expected_failure=$kill_error
        [[ "$kill_error" != 0 ]] || expected_failure=2
        k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" 2>/dev/null || result=$?
        [[ "$result" = "$expected_failure" && "$kills" = $((before_kills + 1)) ]]
        [[ "$stopped" != 'true|0' || "$waits" = 49 ]]
      done
      for stopped_after in 1 49; do
        waits=0; result=0; stopped='true|0'; before_kills=$kills
        k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
        [[ "$result" = 0 && "$waits" = "$stopped_after" && "$kills" = $((before_kills + 1)) ]]
      done
    done
    stopped_after=0; error_after=1; waits=0; stopped='true|0'; result=0; before_kills=$kills
    k3d_hard_stop_node awaken-test k3d-awaken-test-agent-0 "$container" || result=$?
    [[ "$result" = 42 && "$waits" = 1 && "$kills" = $((before_kills + 1)) ]]
  )

  # Node-network fault cause/effect decision table:
  # F1 exact running node/interface + open => one atomic bidirectional block;
  # F2 exact receipt + blocked => exact removal; F3 open + cleanup => no write;
  # F4 malformed/foreign/replaced node, missing/down/ambiguous interface or
  # changed receipt => no write; F5 rule observation/transaction failure =>
  # original error; F6 unexpected/partial rules => refuse mutation; F7 a silent
  # no-op transaction => failed postcondition. The receipt predates F1, allowing
  # cleanup even if observation fails after the effect. Each rule executes the
  # actual helper; the live suite owns packet loss, Ready and product outcomes.
  # F8 after inspecting a name, every exec targets its immutable container ID;
  # the fake rejects named exec so replacement/name reuse cannot redirect effects.
  (
    local node_id="$(printf 'a%.0s' {1..64})" owner=awaken-test role=agent
    local fault_identity fault_addresses fault_rules='' writes=0 transaction_error=0 read_error=0
    local no_effect=0 post_read_error=0 receipt prior malformed
    fault_identity="$node_id|192.168.10.3|02:42:ac:10:00:03"
    fault_addresses='[{"ifname":"eth0","ifindex":2,"address":"02:42:ac:10:00:03","flags":["UP"],"addr_info":[{"family":"inet","local":"192.168.10.3"}]}]'
    docker() {
      case "$*" in
        'inspect --format '*'.Config.Labels'*' -- k3d-awaken-test-agent-0')
          printf '%s|%s\n' "$owner" "$role" ;;
        'inspect --format '*'.NetworkSettings.Networks'*' -- k3d-awaken-test-agent-0')
          printf '%s\n' "$fault_identity" ;;
        "exec $node_id ip -j address show") printf '%s\n' "$fault_addresses" ;;
        "exec $node_id iptables-save -t filter")
          [[ "$read_error" = 0 ]] || return "$read_error"
          printf '%s\n' "$fault_rules" ;;
        "exec -i $node_id iptables-restore --noflush")
          local input
          input=$(cat)
          writes=$((writes + 1))
          [[ "$transaction_error" = 0 ]] || return "$transaction_error"
          [[ "$no_effect" = 0 ]] || return 0
          if [[ "$input" = *'-A AWAKEN-K3D-PARTITION -j DROP'* ]]; then
            # Derive the observed rules from the submitted transaction, ensuring
            # a missing ingress, egress or forwarded rule cannot pass F1.
            fault_rules=$(sed -e '/^\*filter$/d' -e '/^COMMIT$/d' \
              -e 's/^-I \([^ ]*\) 1 /-A \1 /' <<<"$input")
          else
            local removals
            removals=$(grep '^-A \(INPUT\|OUTPUT\|FORWARD\) ' <<<"$fault_rules" | sed 's/^-A /-D /' | sort)
            [[ "$(grep '^-D ' <<<"$input" | sort)" = "$removals" \
              && "$input" = *'-X AWAKEN-K3D-PARTITION'* ]] || return 98
            fault_rules=''
          fi
          read_error=$post_read_error ;;
        *) return 99 ;;
      esac
    }
    receipt=$(k3d_node_network_identity awaken-test k3d-awaken-test-agent-0)
    [[ "$receipt" = "$node_id|eth0|2|192.168.10.3|02:42:ac:10:00:03" ]]
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked
    [[ "$writes" = 1 && -n "$fault_rules" ]]
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked
    [[ "$writes" = 1 ]]
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open
    [[ "$writes" = 2 && -z "$fault_rules" ]]
    # F9 both/ingress/egress × install/remove: two directed jumps only for
    # each one-way profile; incompatible profile and unknown profile perform
    # zero writes. Independent expected text catches a shared-generator bug.
    for direction in ingress egress; do
      k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked "$direction"
      [[ $(grep -c '^-A \(INPUT\|OUTPUT\|FORWARD\) ' <<<"$fault_rules") = 2 ]]
      if [[ "$direction" = ingress ]]; then
        [[ "$fault_rules" = *'-A INPUT -i eth0'* && "$fault_rules" = *'-A FORWARD -i eth0'* && "$fault_rules" != *' -o '* ]]
      else
        [[ "$fault_rules" = *'-A OUTPUT -o eth0'* && "$fault_rules" = *'-A FORWARD -o eth0'* && "$fault_rules" != *' -i '* ]]
      fi
      prior=$writes
      ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open both 2>/dev/null
      [[ "$writes" = "$prior" ]]
      k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open "$direction"
      [[ -z "$fault_rules" ]]
    done
    prior=$writes
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked unknown
    [[ "$writes" = "$prior" ]]
    prior=$writes
    for malformed in '' --all k3d-other-agent-0 k3d-awaken-test-serverlb k3d-awaken-test-agent-x; do
      ! k3d_set_node_network_partition awaken-test "$malformed" "$receipt" blocked 2>/dev/null
    done
    owner=foreign
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked 2>/dev/null
    owner=awaken-test
    role=loadbalancer
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked 2>/dev/null
    role=agent
    for malformed in '' "$node_id|999.1.1.1|02:42:ac:10:00:03" "$node_id|192.168.10.3|bad" "$(printf 'b%.0s' {1..64})|192.168.10.3|02:42:ac:10:00:03"; do
      fault_identity="$malformed"
      ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked 2>/dev/null
    done
    fault_identity="$node_id|192.168.10.3|02:42:ac:10:00:03"
    local valid_addresses="$fault_addresses"
    for fault_addresses in '[]' "${valid_addresses/\"UP\"/\"DOWN\"}" "${valid_addresses/eth0/eth1}" "${valid_addresses/\"ifindex\":2/\"ifindex\":3}" "[${valid_addresses:1:-1},${valid_addresses:1:-1}]"; do
      ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked 2>/dev/null
    done
    fault_addresses="$valid_addresses"
    [[ "$writes" = "$prior" ]]
    read_error=31
    local result=0
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked || result=$?
    [[ "$result" = 31 && "$writes" = "$prior" ]]
    read_error=0
    fault_rules=':AWAKEN-K3D-PARTITION - [0:0]'
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open 2>/dev/null
    [[ "$writes" = "$prior" ]]
    fault_rules=''
    transaction_error=32
    result=0
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked || result=$?
    [[ "$result" = 32 && -z "$fault_rules" ]]
    transaction_error=0
    no_effect=1
    ! k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked
    # F5b post-effect observation failure preserves the prepared receipt and
    # installed fault. F5c failed removal must leave the fault observable; a
    # subsequent exact cleanup succeeds without touching another incarnation.
    no_effect=0
    post_read_error=33
    result=0
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" blocked || result=$?
    [[ "$result" = 33 && -n "$fault_rules" ]]
    read_error=0
    post_read_error=0
    transaction_error=34
    result=0
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open || result=$?
    [[ "$result" = 34 && -n "$fault_rules" ]]
    transaction_error=0
    k3d_set_node_network_partition awaken-test k3d-awaken-test-agent-0 "$receipt" open
    [[ -z "$fault_rules" ]]
  )
  # Name-boundary decision table: N1 ASCII canonical shape and length 1/32
  # => accepted; N2 empty/option/space/uppercase/33-byte/observed 35-byte
  # name => rejected. N2 must reach no capacity, delete/create, import or
  # diagnostic IO. The live CLI rejected the 35-byte name only AFTER our
  # pre-create cleanup; all lifecycle consumers must share this admission.
  local boundary_name invalid_name
  printf -v boundary_name '%032d' 1
  k3d_validate_name a
  k3d_validate_name "$boundary_name"
  for invalid_name in "" --all "with space" Upper "${boundary_name}a" awaken-process-stop-dd4f28-20260909; do
    ! k3d_validate_name "$invalid_name"
    local prior_k3d=${#k3d_calls[@]} prior_capacity=${#capacity_calls[@]} prior_deadlines=${#deadline_calls[@]}
    ! k3d_create_cluster "$invalid_name" 0
    ! k3d_delete_cluster "$invalid_name"
    ! k3d_import_images "$invalid_name" app:latest
    ! k3d_diagnostic_kubectl "$invalid_name" get pods
    (( ${#k3d_calls[@]} == prior_k3d ))
    (( ${#capacity_calls[@]} == prior_capacity ))
    (( ${#deadline_calls[@]} == prior_deadlines ))
  done
  k3d_create_cluster "awaken-test-1" 2
  [[ "${k3d_calls[0]}" = "cluster create awaken-test-1 "* ]]
  [[ "${capacity_calls[${#capacity_calls[@]} - 1]}" = "3 --eviction-threshold 128Mi" ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-minimum-reclaim=imagefs.available=128Mi,nodefs.available=128Mi@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi@agent:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-minimum-reclaim=imagefs.available=128Mi,nodefs.available=128Mi@agent:*"* ]]
  # IG1 admitted offline fixture + large host above kubelet's default 85%
  # image-GC threshold => retain imported images on BOTH node roles. The live
  # failure had 531GiB free at 86% usage: GC removed nginx/busybox and Never-pull
  # Pods could not restart/provision. IG2 server-only topology gets no agent
  # override (checked below); IG3 absolute eviction and capacity rejection stay
  # enforced by T1-T4/K2. This is not a production-cluster GC policy.
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=image-gc-high-threshold=100@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=image-gc-high-threshold=100@agent:*"* ]]
  # Cluster-fixture dependency rule. C1 repository scenarios import their
  # exact images; C2 bundled ingress/metrics are not scenario dependencies.
  # C1+C2 => E1 the canonical builder disables both controllers, preventing
  # unbounded external image-pull retries from perturbing timing-sensitive
  # proofs. K3DH1 exercises the only cluster-construction owner. Lifecycle
  # rule L1 success or failure leaves teardown/retention to the scenario's one
  # installed cleanup owner; K3D must not run a competing automatic rollback.
  [[ "${k3d_calls[0]}" = *"--k3s-arg --disable=traefik@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--k3s-arg --disable=metrics-server@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--no-rollback"* ]]

  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  k3d_create_cluster "awaken-test-1" 0
  [[ "${k3d_calls[0]}" != *"@agent:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=image-gc-high-threshold=100@server:*"* ]]

  # Shared-storage decision table: V1 absent option => no mount; V2 generated
  # ID + existing owned volume => one all-node mount, images alone reach capacity
  # planning; V3 missing/option-shaped/path-shaped/short ID => zero cluster effects;
  # V4 valid ID but absent Docker volume => zero cluster effects. This validates
  # argument admission; the distributed live case owns byte/UID continuity.
  [[ "${k3d_calls[0]}" != *"--volume"* ]]
  local volume_id
  printf -v volume_id '%064d' 1
  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  e2e_docker_create_volume
  [[ "$E2E_CREATED_VOLUME" = "$volume_id" ]]
  k3d_create_cluster "awaken-test" 3 128Mi "" --shared-volume "$volume_id" roles:exact
  [[ "${k3d_calls[0]}" = *"--volume $volume_id:/var/lib/awaken-continuation@all"* ]]
  [[ "${capacity_calls[${#capacity_calls[@]} - 1]}" = \
    "4 --eviction-threshold 128Mi roles:exact" ]]
  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  local bad_volume
  for bad_volume in "" --all /tmp/foo abc; do
    ! k3d_create_cluster "awaken-test" 3 128Mi "" --shared-volume "$bad_volume"
    (( ${#k3d_calls[@]} == 0 ))
  done
  volume_fixture_missing_name="$volume_id"
  ! k3d_create_cluster "awaken-test" 3 128Mi "" --shared-volume "$volume_id"
  (( ${#k3d_calls[@]} == 0 ))
  volume_fixture_missing_name=""

  # Eviction cause/effect table: T1 absent override => one absolute trigger and
  # one absolute recovery margin on server and agent (checked above), independent
  # of host filesystem size; T2 explicit positive quantity => both settings use
  # that quantity on both roles; T3 zero, malformed or percentage input => no
  # cluster effects. T4 capacity failure with otherwise valid arguments => no
  # deletion/create. This prevents a recovered large filesystem from retaining
  # DiskPressure merely because kubelet's percentage minimum reclaim is huge.
  mapfile -t observed_eviction_policy < <(k3d_eviction_policy 2Gi)
  [[ "${observed_eviction_policy[*]}" = \
    "eviction-hard=imagefs.available<2Gi,nodefs.available<2Gi eviction-minimum-reclaim=imagefs.available=2Gi,nodefs.available=2Gi image-gc-high-threshold=100" ]]
  local invalid_eviction
  for invalid_eviction in "" 0Gi 2% low -1Gi; do
    ! k3d_eviction_policy "$invalid_eviction" >/dev/null 2>&1
  done
  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  k3d_create_cluster "awaken-test-1" 1 2Gi
  [[ "${capacity_calls[${#capacity_calls[@]} - 1]}" = "2 --eviction-threshold 2Gi" ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-hard=imagefs.available<2Gi,nodefs.available<2Gi@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-minimum-reclaim=imagefs.available=2Gi,nodefs.available=2Gi@server:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-hard=imagefs.available<2Gi,nodefs.available<2Gi@agent:*"* ]]
  [[ "${k3d_calls[0]}" = *"--kubelet-arg=eviction-minimum-reclaim=imagefs.available=2Gi,nodefs.available=2Gi@agent:*"* ]]

  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  # K2: capacity rejection happens before deleting an existing cluster or
  # creating a new one. The planner's real arithmetic is checked above.
  capacity_failure=1
  ! k3d_create_cluster "awaken-test" 3 128Mi "" roles:exact
  (( ${#k3d_calls[@]} == 0 ))
  [[ "${capacity_calls[${#capacity_calls[@]} - 1]}" = \
    "4 --eviction-threshold 128Mi roles:exact" ]]
  capacity_failure=0
  ! k3d_create_cluster "--all" 1
  ! k3d_create_cluster "awaken-test" many
  ! k3d_create_cluster "awaken-test" 08
  ! k3d_create_cluster "awaken-test" 64
  ! k3d_create_cluster "awaken-test" 18446744073709551616
  ! k3d_create_cluster "awaken-test" 1 low
  ! k3d_create_cluster "awaken-test" 1 0Gi
  ! k3d_create_cluster "awaken-test" 1 0
  ! k3d_create_cluster "awaken-test" 1 2
  ! k3d_create_cluster "awaken-test" 1 '2%'
  (( ${#k3d_calls[@]} == 0 ))
  k3d_create_cluster "awaken-test" 1 "" "k3d-awaken-registry.localhost:5000"
  [[ "${k3d_calls[0]}" = *"--registry-use k3d-awaken-registry.localhost:5000"* ]]
  [[ -z "$K3D_CREATED_CLUSTER" ]] || k3d_delete_cluster "$K3D_CREATED_CLUSTER"
  k3d_calls=()
  ! k3d_create_cluster "awaken-test" 1 "" "--all"
  (( ${#k3d_calls[@]} == 0 ))
  k3d_create_cluster "awaken-test" 0
  for residue in cluster network container volume context; do
    ! k3d_delete_cluster_and_remove "awaken-test"
  done
  residue=""
  # H16 keeps already-absent idempotence. The retired name-based K3D delete
  # error tolerance is gone: failed Docker effects preserve their raw status.
  k3d_delete_cluster "awaken-test"
  local failing_observer
  for failing_observer in k3d docker kubectl; do
    (
      local volume_fixture_dir="$volume_fixture_root/observer-$failing_observer"
      mkdir -p "$volume_fixture_dir"
      observation_error=""
      k3d_create_cluster "awaken-test" 0
      observation_error="$failing_observer"
      ! k3d_delete_cluster_and_remove "awaken-test"
    )
  done
  observation_error=""
  k3d_create_cluster "awaken-test" 0
  local cleanup_artifact
  cleanup_artifact="$(mktemp)"
  residue="cluster"
  ! k3d_delete_cluster_and_remove "awaken-test" "$cleanup_artifact"
  [[ ! -e "$cleanup_artifact" ]]
  residue=""
  k3d_delete_cluster "awaken-test"

  local exit_status
  set +e
  (
    k3d_create_cluster "awaken-test" 0
    exit_cleanup() { k3d_delete_cluster_and_remove "awaken-test"; }
    residue="cluster"
    k3d_install_exit_cleanup exit_cleanup
    true
  )
  exit_status=$?
  set -e
  [[ "$exit_status" = "1" ]]

  set +e
  (
    k3d_create_cluster "awaken-test" 0
    exit_cleanup() { k3d_delete_cluster_and_remove "awaken-test"; }
    residue=""
    k3d_install_exit_cleanup exit_cleanup
    true
  )
  exit_status=$?
  set -e
  [[ "$exit_status" = "0" ]]

  set +e
  (
    k3d_create_cluster "awaken-test" 0
    exit_cleanup() { k3d_delete_cluster_and_remove "awaken-test"; }
    residue=""
    k3d_install_exit_cleanup exit_cleanup
    exit 23
  )
  exit_status=$?
  set -e
  [[ "$exit_status" = "23" ]]

  set +e
  (
    k3d_create_cluster "awaken-test" 0
    exit_cleanup() { k3d_delete_cluster_and_remove "awaken-test"; }
    residue="cluster"
    k3d_install_exit_cleanup exit_cleanup
    exit 23
  )
  exit_status=$?
  set -e
  [[ "$exit_status" = "1" ]]

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
    elif [[ "$1" = ps ]]; then
      printf '%s\n' server agent
    elif [[ "$1 $2" = "image inspect" ]]; then
      image_presence_calls+=("$*")
      [[ "$#" = 5 && "$3" = --format && "$4" = '{{.Id}}' ]] || return 99
      return "$image_observation_status"
    elif [[ "$1" = pull ]]; then
      image_presence_calls+=("$*")
      [[ "$#" = 3 && "$2" = -q ]] || return 99
      return "$image_pull_status"
    elif [[ "$1" = inspect ]]; then
      case "$eviction_observation_fault" in
        gc-missing) ;;
        gc-wrong) printf '%s\n' '--kubelet-arg=image-gc-high-threshold=85' ;;
        gc-duplicate) printf '%s\n' '--kubelet-arg=image-gc-high-threshold=100' '--kubelet-arg=image-gc-high-threshold=100' ;;
        *) printf '%s\n' '--kubelet-arg=image-gc-high-threshold=100' ;;
      esac
      case "$eviction_observation_fault" in
        missing)
          printf '%s\n' \
            server \
            '--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi'
          ;;
        mismatch)
          printf '%s\n' \
            server \
            '--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi' \
            '--kubelet-arg=eviction-minimum-reclaim=imagefs.available=2Gi,nodefs.available=2Gi'
          ;;
        duplicate)
          printf '%s\n' \
            server \
            '--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi' \
            '--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi' \
            '--kubelet-arg=eviction-minimum-reclaim=imagefs.available=128Mi,nodefs.available=128Mi'
          ;;
        *)
          printf '%s\n' \
            server \
            '--kubelet-arg=eviction-hard=imagefs.available<128Mi,nodefs.available<128Mi' \
            '--kubelet-arg=eviction-minimum-reclaim=imagefs.available=128Mi,nodefs.available=128Mi'
          ;;
      esac
    elif [[ "$1 $2" = "build --network" || "$1" = build ]]; then
      docker_builds+=("$*")
    fi
  }
  # Image-presence C/E table: P1 inspect succeeds => Id-only observation, no
  # pull; P2 inspect returns absent(1) or an observation error(37) => preserve
  # the existing pull path; P3 pull fails(41) => return that raw status and do
  # not inspect a later image. No row reads Config/Env/all Labels, and these
  # shell fixtures do not claim a real daemon/image admission result.
  k3d_ensure_images exact:present
  [[ "${image_presence_calls[*]}" = 'image inspect --format {{.Id}} exact:present' ]]
  local presence_status
  for image_observation_status in 1 37; do
    for image_pull_status in 0 41; do
      image_presence_calls=()
      presence_status=0
      k3d_ensure_images exact:missing || presence_status=$?
      [[ "$presence_status" = "$image_pull_status" ]]
      [[ "${image_presence_calls[*]}" = 'image inspect --format {{.Id}} exact:missing pull -q exact:missing' ]]
    done
  done
  image_presence_calls=()
  presence_status=0
  k3d_ensure_images exact:first exact:unreached || presence_status=$?
  [[ "$presence_status" = 41 && "${#image_presence_calls[@]}" = 2 ]]
  [[ "${image_presence_calls[*]}" != *exact:unreached* ]]
  image_observation_status=0 image_pull_status=0
  # The actual exporter subprocess matrix owns save flags and OCI preservation.
  # Here the real shared import function must route each image through that owner,
  # and all post-import Kubernetes effects must use the requested cluster context.
  # This mock rejects ambient-context calls, including when the caller's current
  # context is unavailable or names another cluster.
  k3d_export_image() { docker_saves+=("$1"); }
  kubectl() {
    [[ "$1 $2" = "--context k3d-awaken-test" ]] || return 98
    shift 2
    if [[ "$*" = *"get deploy coredns"* ]]; then
      printf '%s' 'rancher/mirrored-coredns-coredns:1.10.1'
    fi
  }
  k3d_calls=()
  k3d_import_images "awaken-test" app:latest app:latest postgres:16
  (( ${#docker_saves[@]} == 4 ))
  [[ "${docker_saves[*]}" = *"app:latest postgres:16"* ]]
  [[ "${k3d_calls[0]}" = image\ import* ]]
  [[ "${capacity_calls[${#capacity_calls[@]} - 1]}" = \
    "2 --eviction-threshold 128Mi rancher/mirrored-pause:3.6 rancher/mirrored-coredns-coredns:1.10.1 app:latest postgres:16" ]]
  [[ "${k3d_calls[0]}" = *'--mode direct' ]]
  [[ "${docker_execs[*]}" != *'find /k3d/images'* ]]

  # Post-create capacity decision table: I1 the exact running hard and minimum
  # policies agree => import admission uses that quantity; I2 either setting is
  # missing, conflicting or duplicated => fail closed before capacity planning,
  # export or image import. The node command is the authority, so callers cannot
  # accidentally repeat a stale threshold.
  local prior_capacity=${#capacity_calls[@]} prior_saves=${#docker_saves[@]} prior_imports=${#k3d_calls[@]}
  # G1 valid offline-image policy => import; G2 absent/wrong/duplicate GC
  # settings => no capacity admission/export/import. These are independent of
  # the existing missing/mismatched/duplicate absolute eviction rows.
  for eviction_observation_fault in missing mismatch duplicate gc-missing gc-wrong gc-duplicate; do
    ! k3d_import_images "awaken-test" app:latest >/dev/null 2>&1
    (( ${#capacity_calls[@]} == prior_capacity ))
    (( ${#docker_saves[@]} == prior_saves ))
    (( ${#k3d_calls[@]} == prior_imports ))
  done
  eviction_observation_fault=""

  # K2/K3/K5/K6: capacity fails before archive/import; import failure and
  # local archive-cleanup failure each remain failures. The obsolete shared
  # transport store is absent; a failing import still attempts local cleanup.
  capacity_failure=1
  docker_saves=() k3d_calls=()
  ! k3d_import_images "awaken-test" app:latest
  (( ${#docker_saves[@]} == 0 && ${#k3d_calls[@]} == 0 ))
  capacity_failure=0
  local expected_status actual_status archive_cleanups=0 before_cleanups
  # This IO stub removes the actual private directory, then can report the
  # same lost-ACK failure as a filesystem boundary. The original status wins.
  rm() {
    local removal_status=0
    command rm "$@" || removal_status=$?
    if [[ "$1" = -rf && "${2:-}" = -- ]]; then
      archive_cleanups=$((archive_cleanups + 1))
      [[ "$removal_status" = 0 ]] || return "$removal_status"
      return "$archive_cleanup_failure"
    fi
    return "$removal_status"
  }
  for expected_status in 23 31; do
    import_failure=0 archive_cleanup_failure=0
    if [[ "$expected_status" == 23 ]]; then import_failure=23; else archive_cleanup_failure=31; fi
    before_cleanups="$archive_cleanups"
    actual_status=0
    k3d_import_images "awaken-test" app:latest || actual_status=$?
    [[ "$actual_status" == "$expected_status" ]]
    [[ "$archive_cleanups" = "$((before_cleanups + 1))" ]]
  done
  import_failure=0 archive_cleanup_failure=0

  # Build-transport decision table: B1 no override => one default Docker
  # build; B2 host network => only its network changes; B3 an existing CA file
  # => the same build receives a BuildKit secret; B4 missing CA => no build.
  # The secret is an apt trust input, never a copied image layer or runtime
  # credential. These branches share one deadline and one Docker invocation.
  unset AWAKEN_K3D_BUILD_NETWORK AWAKEN_K3D_BUILD_CA_BUNDLE
  k3d_docker_build --load -t fixture:latest .
  [[ "${docker_builds[${#docker_builds[@]} - 1]}" = "build --load -t fixture:latest ." ]]
  [[ "${deadline_calls[${#deadline_calls[@]} - 1]}" = "${K3D_BUILD_TIMEOUT_SECONDS}:docker build --load -t fixture:latest ." ]]
  AWAKEN_K3D_BUILD_NETWORK=host k3d_docker_build --load -t fixture:latest .
  [[ "${docker_builds[${#docker_builds[@]} - 1]}" = "build --network host --load -t fixture:latest ." ]]
  [[ "${deadline_calls[${#deadline_calls[@]} - 1]}" = "${K3D_BUILD_TIMEOUT_SECONDS}:docker build --network host --load -t fixture:latest ." ]]
  local ca_fixture
  ca_fixture=$(mktemp)
  printf 'test CA\n' > "$ca_fixture"
  AWAKEN_K3D_BUILD_NETWORK=host AWAKEN_K3D_BUILD_CA_BUNDLE="$ca_fixture" \
    k3d_docker_build --load -t fixture:latest .
  [[ "${docker_builds[${#docker_builds[@]} - 1]}" = "build --network host --secret id=k3d_apt_ca,src=$ca_fixture --load -t fixture:latest ." ]]
  local builds_before=${#docker_builds[@]}
  if AWAKEN_K3D_BUILD_CA_BUNDLE="$ca_fixture-missing" \
    k3d_docker_build --load -t must-not-run .; then
    return 1
  fi
  [[ "${#docker_builds[@]}" -eq "$builds_before" ]]
  rm -f -- "$ca_fixture"
  local saved_build_timeout="$K3D_BUILD_TIMEOUT_SECONDS"
  K3D_BUILD_TIMEOUT_SECONDS=invalid
  ! k3d_docker_build --load -t must-not-run .
  K3D_BUILD_TIMEOUT_SECONDS="$saved_build_timeout"
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
