#!/usr/bin/env bash
# Real NATS wake and broker-loss polling over the same Postgres dispatch owner.
# Each accepted Thread must have exactly one committed user/assistant pair;
# message counts do not claim exactly-once external execution.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$REPO_ROOT/e2e/k3d/harness.sh"
DEPLOY_DIR="$REPO_ROOT/deploy/k3d"
DRIVER="$REPO_ROOT/e2e/k3d/scaling_driver.ts"
NS="awaken-nats-wake"
RUN_DIR="" CLUSTER="" IMAGE="" PF_PID="" MONITOR_PID="" CLUSTER_ATTEMPTED=0
IMAGE_BUILT=0
LOCAL_PORT="${NATS_WAKE_LOCAL_PORT:-38651}"
M=12

log() { printf '\n=== %s ===\n' "$*"; }

# All three facts come from one Postgres statement/snapshot, not a per-Pod cache.
# Both live-hint and lost-hint batches call this same oracle.
wait_batch() {
  local expected="$1" budget="${2:-120}" deadline remaining last="unobserved"
  deadline=$((SECONDS + budget))
  while (( SECONDS < deadline )); do
    remaining=$((deadline - SECONDS))
    (( remaining <= 20 )) || remaining=20
    if last=$(run_with_deadline "$remaining" kubectl -n "$NS" exec deploy/postgres -- \
      env PGPASSWORD=test psql -U postgres -d awaken -v ON_ERROR_STOP=1 -tA -F ' ' -c \
      'SELECT count(*), count(DISTINCT thread_id), (SELECT count(*) FROM (SELECT thread_id FROM runtime_message GROUP BY thread_id HAVING count(*) <> 2) invalid) FROM runtime_message'); then
      if [[ "$last" = "$((expected * 2)) $expected 0" ]]; then
        printf 'postgres: messages threads invalid = %s\n' "$last"
        return 0
      fi
    fi
    sleep 2
  done
  echo "Postgres batch did not converge: expected=$((expected * 2))/$expected/0 observed=$last" >&2
  return 1
}

submit_batch() {
  local prefix="$1" response status=0
  response=$(run_with_deadline 30 env THREAD_PREFIX="$prefix" node "$DRIVER" \
    submit "http://127.0.0.1:$LOCAL_PORT" "$M" 2>&1) || status=$?
  if (( status != 0 )); then
    printf 'submission failed (status %s): %s\n' "$status" "$response" >&2
    return "$status"
  fi
  [[ "${response##*$'\n'}" = "OK submitted=$M" ]] || {
    printf 'submission receipt mismatch: %s\n' "$response" >&2
    return 1
  }
}

diagnostics() {
  # A failed host build has no cluster context. After a create attempt every
  # diagnostic is bounded and uses only this fixture's private kubeconfig.
  # Failure causes: D1 accepted Threads remain without exact reply pairs;
  # D2 the Brain Pod fails while broker/storage stay Ready; D3 hints are absent
  # or broker delivery stalls. Effects: D1 records canonical dispatch/message
  # coordinates and retry eligibility against the database clock in one
  # PostgreSQL statement, D2 captures the Brain log, and
  # D3 records actual NATS counters. Diagnostic failure never masks the body's
  # original failure. R1 D1+Postgres available => row evidence; R2 D2+Pods
  # available => the Brain log; R3 D3+monitor available => broker snapshot.
  (( CLUSTER_ATTEMPTED == 1 )) || return 0
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get pods -o wide || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" get events --sort-by=.lastTimestamp || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" exec deploy/postgres -- \
    env PGPASSWORD=test psql -U postgres -d awaken -v ON_ERROR_STOP=1 -tA -F ' ' -c \
    "SELECT d.thread_id, d.status, d.attempt_count, d.lease_epoch, d.retry_not_before, (EXTRACT(EPOCH FROM statement_timestamp()) * 1000)::bigint AS database_now_ms, d.lease_owner, d.active_attempt_owner, COALESCE(m.message_count, 0) FROM runtime_dispatch d LEFT JOIN (SELECT thread_id, count(*) AS message_count FROM runtime_message GROUP BY thread_id) m USING (thread_id) WHERE d.thread_id LIKE 'natswake-%' OR d.thread_id LIKE 'natspoll-%' ORDER BY d.thread_id LIMIT 64" || true
  k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" logs -l app=brain \
    --all-containers=true --prefix --tail=80 --max-log-requests=3 || true
  if [[ -n "${MONITOR_PORT:-}" ]] \
    && curl --max-time 2 -fsS "http://127.0.0.1:$MONITOR_PORT/varz" > "$RUN_DIR/nats-failure.json"; then
    python3 "$REPO_ROOT/e2e/k3d/nats_wake_evidence.py" "$RUN_DIR/nats-failure.json" || true
  fi
  local app
  for app in postgres nats; do
    k3d_diagnostic_kubectl "$CLUSTER" -n "$NS" logs "deploy/$app" --tail=60 || true
  done
}

cleanup() {
  local body_status="$1" status=0 remove_status=0 image_status=0 pid
  (( body_status == 0 )) || diagnostics
  for pid in "$PF_PID" "$MONITOR_PID"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  if (( CLUSTER_ATTEMPTED == 1 )); then
    k3d_delete_cluster "$CLUSTER" || status=$?
  fi
  if (( IMAGE_BUILT == 1 )); then
    run_with_deadline "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS" docker image rm "$IMAGE" >/dev/null || image_status=$?
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR" || remove_status=$?
  (( status == 0 )) || return "$status"
  (( image_status == 0 )) || return "$image_status"
  return "$remove_status"
}

# One observation owner for readiness and post-submit progress. The broker is
# private to this fleet, so message deltas cannot come from another test run.
# Polling may also drain work; this proves actual NATS publication/delivery,
# not that every committed Run was causally awakened by its particular hint.
wait_nats() {
  local baseline="${1:-}" budget="${2:-30}" deadline=$((SECONDS + ${2:-30}))
  while (( SECONDS < deadline )); do
    if curl --max-time 2 -fsS "http://127.0.0.1:$MONITOR_PORT/varz" > "$RUN_DIR/nats-current.json" \
      && python3 "$REPO_ROOT/e2e/k3d/nats_wake_evidence.py" "$RUN_DIR/nats-current.json" ${baseline:+"$baseline"}; then
      return 0
    fi
    sleep 1
  done
  echo "NATS evidence did not converge within ${budget}s" >&2
  return 1
}

render_overlay() {
  # Reuse the canonical topology and Kustomize image transformer. No copied
  # Deployment or hand-maintained YAML replacement becomes a second authority.
  python3 - "$DEPLOY_DIR/nats-wake" "$CLUSTER" "$NS" "$RUN_DIR/kustomization.yaml" <<'OVERLAY'
import json, os, sys
from pathlib import Path
Path(sys.argv[4]).write_text(json.dumps({
    "apiVersion": "kustomize.config.k8s.io/v1beta1", "kind": "Kustomization",
    "namespace": sys.argv[3], "resources": [os.path.relpath(sys.argv[1], Path(sys.argv[4]).parent)],
    "images": [{"name": "awaken-topology", "newName": sys.argv[2], "newTag": "local"}],
}))
OVERLAY
}

main() {
  cd "$REPO_ROOT"
  M="${1:-12}"
  # Fixture admission: canonical bounded integer only, before any build/cluster
  # effect. The production workload has no dependency on this test batch bound.
  [[ "$M" =~ ^[1-9][0-9]{0,3}$ ]] && (( M <= 1000 )) || {
    echo 'NATS test batch must be an integer from 1 to 1000' >&2
    return 2
  }
  [[ "$LOCAL_PORT" =~ ^[1-9][0-9]{3,4}$ ]] && (( LOCAL_PORT >= 1024 && LOCAL_PORT < 65535 )) || {
    echo 'NATS_WAKE_LOCAL_PORT must be a TCP port from 1024 to 65534' >&2
    return 2
  }
  k3d_admit_or_exit "NATS wake E2E"
  RUN_DIR=$(mktemp -d "${TMPDIR:-/tmp}/awaken-nats.XXXXXX")
  CLUSTER="awaken-nats-$(printf '%s' "$RUN_DIR" | sha256sum | cut -c1-20)"
  IMAGE="$CLUSTER:local"
  export KUBECONFIG="$RUN_DIR/kubeconfig"
  touch "$KUBECONFIG"
  k3d_install_exit_cleanup cleanup

  log 'build current scenario host with the production NATS adapter'
  resolve_cargo_executable awaken-scenario-host awaken-scenario-host --features nats \
    --snapshot-destination "$RUN_DIR/awaken-server" >/dev/null
  cp "$DEPLOY_DIR/Dockerfile.server" "$RUN_DIR/Dockerfile"
  k3d_docker_build --load -q -t "$IMAGE" "$RUN_DIR" >/dev/null
  IMAGE_BUILT=1
  # Include every imported workload image in admission before cluster effects.
  k3d_ensure_images "$IMAGE" postgres:16 nats:2
  CLUSTER_ATTEMPTED=1
  k3d_create_cluster "$CLUSTER" 0 "" "" "$IMAGE" postgres:16 nats:2
  k3d kubeconfig get "$CLUSTER" > "$KUBECONFIG"
  k3d_import_images "$CLUSTER" "$IMAGE" postgres:16 nats:2
  kubectl create namespace "$NS" >/dev/null
  render_overlay
  kubectl apply -k "$RUN_DIR" >/dev/null
  kubectl -n "$NS" rollout status deploy/nats --timeout=120s
  kubectl -n "$NS" rollout status deploy/postgres --timeout=120s
  kubectl -n "$NS" rollout status deploy/brain --timeout=150s

  LOCAL_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
  PF_PID=$(k3d_start_port_forward "$NS" svc/brain "$LOCAL_PORT" 3000 "$RUN_DIR/port-forward.log")
  local ready=0
  for _ in $(seq 1 60); do
    if curl --max-time 2 -fsS -o /dev/null "http://127.0.0.1:$LOCAL_PORT/v1/durable/threads/probe/messages" 2>/dev/null; then
      ready=1; break
    fi
    sleep 1
  done
  (( ready == 1 )) || { cat "$RUN_DIR/port-forward.log" >&2; return 1; }
  MONITOR_PORT=$(k3d_available_port "$((LOCAL_PORT + 1))")
  MONITOR_PID=$(k3d_start_port_forward "$NS" svc/nats "$MONITOR_PORT" 8222 "$RUN_DIR/monitor-forward.log")
  wait_nats
  cp "$RUN_DIR/nats-current.json" "$RUN_DIR/nats-before.json"

  # S6 cause/effect table: C1 Postgres authority available; C2 NATS available at
  # startup; C3 NATS available after enqueue; C4 unique accepted Thread per run.
  # S6-1 C1+C2+C3+C4 => committed reply pairs AND observed broker delivery.
  # S6-2 C1+C2+!C3+C4 => same durable result through bounded poll fallback.
  # S6-3 !C2 => explicit startup failure (owned by configuration/adapter tests).
  # S6-4 !C1 => no durable success (owned by the strict PostgreSQL suite).
  # K1 one co-located Brain owns this Scenario Session/Resource projection;
  # the separate distributed Control E2E owns cross-replica Session recovery.
  # A valid submit receipt AND process exit zero are prerequisites for both
  # batches. The shared SQL oracle requires total, distinct and per-Thread counts.
  log "live NATS: submit $M distinct Threads"
  submit_batch natswake
  wait_batch "$M"
  wait_nats "$RUN_DIR/nats-before.json"
  log 'remove NATS; the same fleet must drain through durable polling'
  kubectl -n "$NS" scale deployment/nats --replicas=0 >/dev/null
  kubectl -n "$NS" wait --for=delete pod -l app=nats --timeout=60s
  submit_batch natspoll
  wait_batch "$((M * 2))"
  log "NATS wake E2E PASSED: $M live-hint and $M broker-loss Threads"
}

if [[ "${BASH_SOURCE[0]}" = "$0" ]]; then
  # The batch size is the only positional, and --self-test the only flag. A
  # rejection has to name the calling convention: "must be an integer" alone
  # does not tell a caller that --self-test exists or that nothing else does.
  case "${1:-}" in
    ""|--self-test) ;;
    -*)
      echo "usage: e2e/k3d/nats_wake_e2e.sh [--self-test | <batch 1..1000>]" >&2
      exit 2
      ;;
  esac
  if [[ "${1:-}" = --self-test ]]; then
    bash "$REPO_ROOT/e2e/k3d/nats_wake_contract_test.sh"
  else
    main "$@"
  fi
fi
