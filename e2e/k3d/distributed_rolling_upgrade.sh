#!/usr/bin/env bash
# Sourced by distributed_control_e2e.sh. The parent owns the cluster and images.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo 'usage: source distributed_rolling_upgrade.sh from distributed_control_e2e.sh' >&2
  exit 2
fi

rolling_k() { kubectl -n "$NS" "$@"; }
rolling_pg() {
  rolling_k exec postgres-primary-0 -- psql -U postgres -d runtime -At -v ON_ERROR_STOP=1 "$@"
}

rolling_pull_release_images() {
  local image role i
  local -a roles=(coordinator coordinator worker worker)
  local -a images=("$ROLLING_LEGACY_IMAGE" "$ROLLING_CANDIDATE_IMAGE"
                   "$ROLLING_LEGACY_WORKER_IMAGE" "$ROLLING_CANDIDATE_WORKER_IMAGE")
  for image in "${images[@]}"; do
    if [[ "$image" != ghcr.io/* ]] || ! k3d_validate_image_digest "$image"; then
      err "rolling release verification requires GHCR role image digests"
      return 2
    fi
  done
  for i in "${!images[@]}"; do
    image="${images[i]}" role="${roles[i]}"
    # Always contact GHCR so a cached image cannot mask a broken pull grant.
    docker pull --platform "$K3D_PLATFORM" "$image" || return "$?"
    docker run --rm --network none --entrypoint "/usr/local/bin/awaken-$role" \
      "$image" --help >/dev/null || return "$?"
    if [[ "$role" == worker ]]; then
      docker run --rm --network none --entrypoint python3 \
        "$image" --version >/dev/null || return "$?"
    fi
  done
}

rolling_image_pair() {
  local role="$1" legacy="$2" candidate="$3" legacy_id candidate_id
  legacy_id=$(docker image inspect --format '{{.Id}}' "$legacy")
  candidate_id=$(docker image inspect --format '{{.Id}}' "$candidate")
  [[ "$legacy_id" != "$candidate_id" ]] || { err "$role references name the same image"; return 1; }
  echo "rolling $role images: $legacy $legacy_id / $candidate $candidate_id"
}

rolling_overlay() {
  local kind name image
  printf 'patches:\n' >>"$ROLE_OVERLAY/kustomization.yaml"
  while read -r kind name image; do
    cat >>"$ROLE_OVERLAY/kustomization.yaml" <<YAML
  - target: {kind: $kind, name: ^${name}$}
    patch: |
      - op: replace
        path: /spec/template/spec/containers/0/image
        value: "$image"
YAML
    if [[ "$kind" = StatefulSet && "$ROLLING_REQUIRE_RELEASE" = 1 ]]; then
      # The published Worker runs as UID 10001 and writes its mounted data
      # directory. The offline fixture supplies that group's volume access.
      cat >>"$ROLE_OVERLAY/kustomization.yaml" <<'YAML'
      - op: add
        path: /spec/template/spec/securityContext
        value: {fsGroup: 10001}
YAML
    fi
  done <<TARGETS
Deployment coordinator $ROLLING_LEGACY_IMAGE
StatefulSet worker $ROLLING_LEGACY_WORKER_IMAGE
Job coordinator-migrate $ROLLING_LEGACY_IMAGE
TARGETS
}

rolling_start_observer() {
  rolling_k create configmap rolling-observer \
    --from-file=observer.py="$REPO_ROOT/e2e/k3d/rolling_outbox_observer.py" >/dev/null
  rolling_k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: rolling-observer
  labels: {app: rolling-observer}
spec:
  automountServiceAccountToken: false
  containers:
    - name: observer
      image: $IMAGE
      imagePullPolicy: Never
      command: [python3, /opt/fixture/observer.py]
      readinessProbe: {httpGet: {path: /healthz, port: 3001}}
      volumeMounts: [{name: observer, mountPath: /opt/fixture, readOnly: true}]
  volumes:
    - name: observer
      configMap: {name: rolling-observer}
---
apiVersion: v1
kind: Service
metadata: {name: rolling-observer}
spec:
  selector: {app: rolling-observer}
  ports: [{port: 3001}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: rolling-observer}
spec:
  podSelector: {matchLabels: {app: rolling-observer}}
  policyTypes: [Ingress, Egress]
  ingress:
    - from: [{podSelector: {matchLabels: {app: coordinator}}}]
      ports: [{protocol: TCP, port: 3001}]
  egress:
    - to: [{podSelector: {matchLabels: {app: control}}}]
      ports: [{protocol: TCP, port: 3001}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: rolling-coordinator-egress}
spec:
  podSelector: {matchLabels: {app: coordinator}}
  policyTypes: [Egress]
  egress:
    - to: [{podSelector: {matchLabels: {app: rolling-observer}}}]
      ports: [{protocol: TCP, port: 3001}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: rolling-control-ingress}
spec:
  podSelector: {matchLabels: {app: control}}
  policyTypes: [Ingress]
  ingress:
    - from: [{podSelector: {matchLabels: {app: rolling-observer}}}]
      ports: [{protocol: TCP, port: 3001}]
YAML
  rolling_k wait --for=condition=ready pod/rolling-observer --timeout=180s
}

rolling_gate() {
  rolling_k exec rolling-observer -- python3 -c '
import sys, urllib.request
request = urllib.request.Request("http://127.0.0.1:3001/" + sys.argv[1], method="POST")
print(urllib.request.urlopen(request, timeout=10).read().decode())
' "$1"
}

rolling_inventory() {
  rolling_k get pods -l 'app in (coordinator,worker)' -o json | python3 -c '
import json, sys
for pod in json.load(sys.stdin)["items"]:
    role = pod["metadata"]["labels"]["app"]
    if pod["metadata"].get("deletionTimestamp") or not any(
        c["type"] == "Ready" and c["status"] == "True" for c in pod["status"].get("conditions", [])
    ):
        continue
    container = next(c for c in pod["status"]["containerStatuses"] if c["name"] == role)
    spec = next(c for c in pod["spec"]["containers"] if c["name"] == role)
    print(role, pod["metadata"]["name"], pod["status"]["podIP"], spec["image"], container["imageID"], sep="\t")
'
}

rolling_wait_inventory() {
  local name="$1" old_coord="$2" new_coord="$3" old_worker="$4" new_worker="$5"
  local deadline=$((SECONDS + 180)) actual expected
  expected="$old_coord $new_coord $old_worker $new_worker"
  while (( SECONDS < deadline )); do
    rolling_inventory >"$evidence/pods-$name.tsv"
    actual=$(awk -F '\t' -v old="$ROLLING_LEGACY_IMAGE" -v new="$ROLLING_CANDIDATE_IMAGE" \
      -v old_worker="$ROLLING_LEGACY_WORKER_IMAGE" -v new_worker="$ROLLING_CANDIDATE_WORKER_IMAGE" '
      $1=="coordinator" && $4==old { oc++ }
      $1=="coordinator" && $4==new { nc++ }
      $1=="worker" && $4==old_worker { ow++ }
      $1=="worker" && $4==new_worker { nw++ }
      END { printf "%d %d %d %d",oc,nc,ow,nw }' "$evidence/pods-$name.tsv")
    if [[ "$actual" == "$expected" ]]; then
      cat "$evidence/pods-$name.tsv"
      return
    fi
    if [[ -f "$evidence/stop" ]]; then
      break
    fi
    sleep 1
  done
  err "$name expected $expected ready legacy/candidate Coordinator/Worker Pods, last saw $actual"
  return 1
}

rolling_progress() {
  local phase="$1" deadline=$((SECONDS + 120))
  while (( SECONDS < deadline )) && [[ ! -f "$evidence/stop" ]]; do
    rolling_k logs rolling-observer >"$evidence/outbox.jsonl"
    if python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" progress "$evidence" "$phase"; then
      return 0
    fi
    sleep 1
  done
  err "$phase did not advance both confirmed turns and the Outbox under load"
  return 1
}

rolling_stage() {
  log "rolling upgrade: $1"
  printf '%s\n' "$1" >"$evidence/stage-next"
  mv "$evidence/stage-next" "$evidence/stage"
}

rolling_hold_fact() {
  local deadline=$((SECONDS + 120)) fact
  rolling_gate arm >/dev/null
  while (( SECONDS < deadline )) && [[ ! -f "$evidence/stop" ]]; do
    fact=$(rolling_gate held | python3 -c 'import json,sys; print(json.load(sys.stdin) or "")')
    if [[ "$fact" =~ ^[[:alnum:]_:-]+$ ]]; then
      test "$(rolling_pg -c "SELECT count(*) FROM managed_lifecycle_outbox WHERE fact_id = '$fact'")" = 1 || return 1
      printf '%s\n' "$fact"
      return
    fi
    sleep 1
  done
  err "no successfully forwarded fact is awaiting an Outbox acknowledgement" >&2
  return 1
}

rolling_release_fact() {
  local fact="$1" prior="$2" next="$3" deadline=$((SECONDS + 120))
  [[ "$(rolling_gate release)" == "\"$fact\"" ]]
  while (( SECONDS < deadline )) && [[ ! -f "$evidence/stop" ]]; do
    rolling_k logs rolling-observer >"$evidence/outbox.jsonl"
    if python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" replay "$evidence" "$fact" "$prior" "$next" &&
        [[ "$(rolling_pg -c "SELECT count(*) FROM managed_lifecycle_outbox WHERE fact_id = '$fact'")" = 0 ]]; then
      return 0
    fi
    sleep 1
  done
  err "no acknowledged replay of $fact across $prior and $next"
  return 1
}

rolling_migrate() {
  rolling_k get job coordinator-migrate -o json | python3 -c '
import json, sys
original = json.load(sys.stdin)
spec = original["spec"]
spec.pop("selector", None)
spec.pop("manualSelector", None)
template = spec["template"]
template["metadata"] = {"labels": {"app": "coordinator-migrate"}}
template["spec"]["containers"][0]["image"] = sys.argv[1]
spec["suspend"] = False
print(json.dumps({"apiVersion": "batch/v1", "kind": "Job",
                  "metadata": {"name": "coordinator-migrate-candidate"}, "spec": spec}))
' "$ROLLING_CANDIDATE_IMAGE" | rolling_k create -f -
  rolling_k wait --for=condition=complete job/coordinator-migrate-candidate --timeout=240s
  rolling_k logs job/coordinator-migrate-candidate >"$evidence/migration.log"
}

rolling_start_claim() {
  local phase="$1" deadline=$((SECONDS + 90)) info session claim="" answer
  ADR71_AGENT_ID=adr71-rolling node "$DRIVER" rolling-claim-probe "$API_URL" "$evidence" "$phase" \
    >"$evidence/claim-$phase.log" 2>&1 &
  claim_pid=$!
  info="$evidence/claim-$phase.json"
  while (( SECONDS < deadline )) && [[ ! -f "$evidence/stop" ]] && kill -0 "$claim_pid" 2>/dev/null; do
    if [[ -s "$info" ]]; then
      session=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sessionId"])' "$info")
      [[ "$session" =~ ^[[:alnum:]_:-]+$ ]] || { err "unexpected claim Session ID: $session"; return 1; }
      claim=$(rolling_pg -c "
        SELECT json_build_object('run_id',run_id,'owner',lease_owner,'epoch',lease_epoch)
        FROM runtime_dispatch
        WHERE status = 'running' AND lease_owner IS NOT NULL
          AND position('\"$session\"' in request::text) > 0
        LIMIT 2")
      if [[ -n "$claim" ]]; then break; fi
    fi
    sleep 1
  done
  if [[ -z "$claim" ]]; then
    err "no durable live claim for $phase"
    cat "$evidence/claim-$phase.log" >&2
    return 1
  fi
  printf '%s\n' "$claim" >"$evidence/claim-$phase-observed.json"
  answer=$(rolling_probe_claim "$phase" /v1/worker/dispatch/claim_is_current "$claim")
  printf '%s\n' "$answer" >"$evidence/claim-$phase-live.json"
  printf '%s' "$answer" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["status"] == 200 and r["body"]["current"] is True, r'
}

rolling_probe_claim() {
  local phase="$1" route="$2" claim="$3" session="${4:-}" owner ip pod
  owner=$(printf '%s\n' "$claim" | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner"])')
  IFS=: read -r _ _ pod _ <<<"$owner"
  ip=$(awk -F '\t' '$1 == "coordinator" { print $3; exit }' "$evidence/pods-$phase.tsv")
  rolling_k exec -i "$pod" -c worker -- python3 - "$ip" "$route" "$claim" "$session" \
    <"$REPO_ROOT/e2e/k3d/rolling_claim_probe.py"
}

rolling_probe_attribution() {
  local phase="$1" after="$2" claim="$3" session image_id answer deadline=$((SECONDS + 20))
  session=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sessionId"])' "$evidence/claim-$phase.json")
  image_id=$(awk -F '\t' '$1 == "coordinator" { print $5; exit }' "$evidence/pods-$after.tsv")
  answer=$(rolling_probe_claim "$after" /v1/worker/session/model-request/admit "$claim" "$session")
  printf '%s\n' "$answer" >"$evidence/claim-$phase-model.json"
  printf '%s' "$answer" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["status"] >= 400 and "stale" in json.dumps(r["body"]), r'
  while (( SECONDS < deadline )); do
    if python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" verify-model-trace \
        "$evidence" "$claim" "$session" "$image_id"; then return 0; fi
    sleep 1
  done
  err "missing Coordinator image evidence for the rejected model-admission probe"
  return 1
}

rolling_finish_claim() {
  local phase="$1" after="$2" claim answer
  claim=$(cat "$evidence/claim-$phase-observed.json")
  wait "$claim_pid"
  claim_pid=
  answer=$(rolling_probe_claim "$after" /v1/worker/dispatch/claim_is_current "$claim")
  printf '%s\n' "$answer" >"$evidence/claim-$phase-stale.json"
  printf '%s' "$answer" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["status"] == 200 and r["body"]["current"] is False, r'
  if [[ "$after" != rollback ]]; then
    answer=$(rolling_probe_claim "$after" /v1/worker/checkpoint/get "$claim")
    printf '%s\n' "$answer" >"$evidence/claim-$phase-checkpoint.json"
    printf '%s' "$answer" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["status"] == 409 and "checkpoint" not in r["body"], r'
    rolling_probe_attribution "$phase" "$after" "$claim"
  fi
  log "$phase to $after: exact live claim became stale after its acknowledged commit"
}

rolling_run_states() {
  # Link the unique public test input to the Run retained with that exact
  # Session root. No trace fields are inferred from a rollout phase or Pod IP.
  rolling_pg -c "
      SELECT json_build_object('sessionId', root.session_id, 'marker', block->>'text',
        'runId', entry->'event'->>'run_id', 'runState', (
          SELECT runtime.payload::jsonb->'state'
          FROM runtime_event runtime
          WHERE runtime.run_id = entry->'event'->>'run_id'
            AND runtime.kind::text = '\"RunStateChanged\"'
          ORDER BY runtime.sequence DESC LIMIT 1
        ))
      FROM managed_session root
      CROSS JOIN LATERAL jsonb_array_elements(COALESCE(
        root.aggregate_json::jsonb#>'{aggregate,event_batches}',
        root.aggregate_json::jsonb->'event_batches', '[]'::jsonb)) batch
      CROSS JOIN LATERAL jsonb_array_elements(batch->'events') entry
      CROSS JOIN LATERAL jsonb_array_elements(entry->'event'->'content') block
      WHERE entry->'event'->>'kind' = 'user_message'
        AND block->>'text' ~ '^ADR71-ROLLING-[0-9]+-[0-9]+$'" \
    >"$evidence/receipt-run-links.jsonl"
}

rolling_attribution() {
  if [[ ! -f "$evidence/receipt-run-links.jsonl" ]] &&
      grep -Eq '"kind"[[:space:]]*:[[:space:]]*"(terminal-error|observation-failure)"' "$evidence/traffic.jsonl"; then
    rolling_run_states || return "$?"
  fi
  python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" attribution "$evidence"
}

rolling_stop_pod_observer() {
  local status=0
  kill "$pod_observer_pid" 2>/dev/null || status=1
  wait "$pod_observer_pid" || status=1
  pod_observer_pid=""
  return "$status"
}

rolling_run() {
  local upgrading rollback result=0
  rolling_wait_inventory legacy 2 0 2 0
  ADR71_AGENT_ID=adr71-rolling node "$DRIVER" prepare-dispatch-load "$API_URL"
  rolling_stage legacy
  ADR71_AGENT_ID=adr71-rolling node "$DRIVER" rolling-load "$API_URL" "$evidence" >"$evidence/load.log" 2>&1 &
  load_pid=$!
  rolling_progress legacy
  upgrading=$(rolling_hold_fact)

  rolling_stage migration
  rolling_migrate
  rolling_wait_inventory migration 2 0 2 0
  rolling_progress migration
  rolling_start_claim migration
  # Keep a Ready old and new Coordinator in service before progressing the rollout.
  rolling_k patch deploy/coordinator --type=merge -p \
    '{"spec":{"minReadySeconds":30,"strategy":{"type":"RollingUpdate","rollingUpdate":{"maxSurge":1,"maxUnavailable":0}}}}'
  rolling_stage upgrading-coordinator
  rolling_k set image deploy/coordinator "coordinator=$ROLLING_CANDIDATE_IMAGE"
  rolling_wait_inventory mixed-coordinator 2 1 2 0
  rolling_stage mixed-coordinator
  rolling_progress mixed-coordinator
  rolling_k rollout status deploy/coordinator --timeout=180s
  rolling_wait_inventory upgraded-coordinator 0 2 2 0
  rolling_stage upgraded-coordinator
  rolling_progress upgraded-coordinator
  rolling_release_fact "$upgrading" legacy upgraded-coordinator
  rolling_finish_claim migration upgraded-coordinator

  rolling_stage upgrading-worker
  rolling_k patch statefulset/worker --type=merge -p '{"spec":{"updateStrategy":{"rollingUpdate":{"partition":1}}}}'
  rolling_k set image statefulset/worker "worker=$ROLLING_CANDIDATE_WORKER_IMAGE"
  rolling_wait_inventory mixed-worker 0 2 1 1
  rolling_stage mixed-worker
  rolling_progress mixed-worker
  rolling_stage upgrading-worker-rest
  rolling_k patch statefulset/worker --type=merge -p '{"spec":{"updateStrategy":{"rollingUpdate":{"partition":0}}}}'
  rolling_k rollout status statefulset/worker --timeout=360s
  rolling_wait_inventory candidate 0 2 0 2
  rolling_stage candidate
  rolling_progress candidate
  rollback=$(rolling_hold_fact)
  [[ "$rollback" != "$upgrading" ]]

  rolling_stage rollback-worker
  rolling_k set image statefulset/worker "worker=$ROLLING_LEGACY_WORKER_IMAGE"
  # StatefulSet retires both replicas in sequence, including the Coordinator's
  # observation of each drained process before its replacement can start.
  rolling_k rollout status statefulset/worker --timeout=360s
  rolling_wait_inventory rollback-worker 0 2 2 0
  rolling_stage rolled-back-worker
  rolling_progress rolled-back-worker
  rolling_start_claim rollback-worker
  rolling_stage rolling-back-coordinator
  rolling_k set image deploy/coordinator "coordinator=$ROLLING_LEGACY_IMAGE"
  rolling_k rollout status deploy/coordinator --timeout=180s
  rolling_wait_inventory rollback 2 0 2 0
  rolling_stage rollback
  rolling_progress rollback
  rolling_release_fact "$rollback" candidate rollback
  rolling_finish_claim rollback-worker rollback
  touch "$evidence/stop"
  wait "$load_pid" || result=1
  load_pid=
  rolling_k logs rolling-observer >"$evidence/outbox.jsonl"
  [[ -z "$(rolling_k get pods -l app=awaken-sandbox -o name)" ]]
  rolling_stop_pod_observer || result=1
  python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" retirement "$evidence" \
    "$ROLLING_LEGACY_WORKER_IMAGE" "$ROLLING_CANDIDATE_WORKER_IMAGE" || result=1
  if rolling_run_states; then
    python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" durable-runs "$evidence" || result=1
  else
    result=1
  fi
  rolling_attribution || result=1
  python3 "$REPO_ROOT/e2e/k3d/rolling_upgrade_audit.py" receipts "$evidence" || result=1
  return "$result"
}

rolling_upgrade() (
  local evidence="$ADR71_ROLLING_EVIDENCE_DIR" load_pid="" claim_pid="" pod_observer_pid=""
  mkdir "$evidence"
  trap '
    status=$?
    trap - EXIT
    touch "$evidence/stop"
    for pid in "$load_pid" "$claim_pid"; do
      if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    if [[ -n "$pod_observer_pid" ]]; then rolling_stop_pod_observer || true; fi
    if [[ -s "$evidence/traffic.jsonl" && ! -f "$evidence/failure-attribution.json" ]]; then
      rolling_attribution || true
    fi
    rolling_k logs rolling-observer >"$evidence/outbox.jsonl" || true
    if (( status != 0 )); then tail -30 "$evidence/load.log" >&2 || true; fi
    exit "$status"
  ' EXIT
  python3 "$REPO_ROOT/e2e/k3d/rolling_pod_observer.py" "$NS" "$evidence" >"$evidence/pod-observer.log" 2>&1 &
  pod_observer_pid=$!
  rolling_run
)
