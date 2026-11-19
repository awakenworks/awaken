# Sourced by distributed_control_e2e.sh after it admits an owned k3d cluster.

preview_k() { kubectl -n "$NS" "$@"; }
preview_pg() { preview_k exec -i postgres-primary-0 -- psql -U postgres -d runtime -v ON_ERROR_STOP=1 -At "$@"; }

session_preview_prepare() {
  PREVIEW_EVIDENCE=$(realpath -m "$PREVIEW_EVIDENCE")
  [[ "$PREVIEW_EVIDENCE/" != "$REPO_ROOT/"* ]] || { err 'preview evidence belongs outside the checkout'; return 2; }
  mkdir -p "$PREVIEW_EVIDENCE"
  [[ ! -e "$PREVIEW_EVIDENCE/results.ndjson" ]] || { err 'preview evidence directory already used'; return 2; }
  export ADR71_PREVIEW_EVIDENCE_DIR="$PREVIEW_EVIDENCE"
  preview_k create configmap session-preview-provider --from-file=provider.py="$REPO_ROOT/e2e/k3d/session_preview_provider.py"
  preview_k patch deploy/provider --type=strategic --patch-file /dev/stdin <<'YAML'
spec:
  replicas: 1
  template:
    spec:
      containers:
        - name: provider
          command: [python3, /fixture/provider.py]
          args: []
          volumeMounts: [{name: preview-fixture, mountPath: /fixture, readOnly: true}]
      volumes: [{name: preview-fixture, configMap: {name: session-preview-provider}}]
YAML
  preview_k get secret adr71-role-config -o json | python3 -c '
import base64, json, sys
data=json.load(sys.stdin)["data"]
config=base64.b64decode(data["coordinator.toml"]).decode()
worker=base64.b64decode(data["worker.toml"]).decode()
assert config.count("[worker_kubernetes]") == 1
settings="preview_nats_url = \"nats://session-preview-nats:4222\"\npreview_nats_subject = \"session-preview\"\n"
print(json.dumps({"stringData":{
    "coordinator.toml":config.replace("[worker_kubernetes]", settings+"[worker_kubernetes]"),
    "worker.toml":"log_filter = \"warn,awaken_coordinator_client=debug\"\n"+worker,
}}))
' | preview_k patch secret adr71-role-config --type=merge --patch-file /dev/stdin
  preview_k get configmap adr71-worker-tls-route -o json | python3 -c '
import json, sys
config=json.load(sys.stdin)["data"]["nginx.conf"]
assert config.count("location / {") == 1
route="""location = /v1/worker/dispatch/stream_batch {
        error_page 418 = @hold_preview;
        if (-f /tmp/hold-preview) { return 418; }
        proxy_pass http://127.0.0.1:3001;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Connection "";
      }
      location @hold_preview {
        proxy_pass http://provider:3000;
        proxy_read_timeout 3s;
      }
      """
config=config.replace("$http_x_awaken_worker_incarnation\x27;", "$http_x_awaken_worker_incarnation $request_time\x27;")
print(json.dumps({"data":{"nginx.conf":config.replace("location / {", route+"location / {")}}))
' | preview_k patch configmap adr71-worker-tls-route --type=merge --patch-file /dev/stdin
  # The Service exists but has no endpoints while the real Coordinators start.
  preview_k apply -f - <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: session-preview-nats}
spec:
  replicas: 0
  selector: {matchLabels: {app: session-preview-nats}}
  template:
    metadata: {labels: {app: session-preview-nats}}
    spec:
      containers:
        - name: nats
          image: nats:2
          imagePullPolicy: Never
          ports: [{containerPort: 4222}]
          readinessProbe: {tcpSocket: {port: 4222}}
          resources: {requests: {cpu: 100m, memory: 32Mi}, limits: {cpu: '1', memory: 128Mi}}
---
apiVersion: v1
kind: Service
metadata: {name: session-preview-nats}
spec:
  selector: {app: session-preview-nats}
  ports: [{port: 4222, targetPort: 4222}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: session-preview-coordinator-egress}
spec:
  podSelector: {matchLabels: {app: coordinator}}
  policyTypes: [Egress]
  egress:
    - to: [{podSelector: {matchLabels: {app: session-preview-nats}}}]
      ports: [{protocol: TCP, port: 4222}]
    - to: [{podSelector: {matchLabels: {app: provider}}}]
      ports: [{protocol: TCP, port: 3000}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: session-preview-fixture-ingress}
spec:
  podSelector:
    matchExpressions: [{key: app, operator: In, values: [session-preview-nats, provider]}]
  policyTypes: [Ingress]
  ingress:
    - from: [{podSelector: {matchLabels: {app: coordinator}}}]
      ports: [{protocol: TCP, port: 4222}, {protocol: TCP, port: 3000}]
YAML
}

session_preview_versions() {
  local coordinator=$1 worker=$2 fresh=${3:-0} old_coordinator old_worker
  preview_k patch service/coordinator --type=merge -p '{"spec":{"selector":{"preview-ingress":null}}}' >/dev/null
  old_coordinator=$(preview_k get deploy/coordinator -o jsonpath='{.spec.template.spec.containers[?(@.name=="coordinator")].image}')
  old_worker=$(preview_k get statefulset/worker -o jsonpath='{.spec.template.spec.containers[?(@.name=="worker")].image}')
  if [[ "$coordinator" != "$old_coordinator" ]]; then
    preview_k set image deploy/coordinator "coordinator=$coordinator" >/dev/null
  elif [[ "$fresh" = 1 ]]; then
    preview_k rollout restart deploy/coordinator >/dev/null
  fi
  preview_k rollout status deploy/coordinator --timeout=420s
  session_preview_run route before-worker-rollout
  if [[ "$worker" != "$old_worker" ]]; then
    preview_k set image statefulset/worker "worker=$worker" >/dev/null
  else
    preview_k rollout restart statefulset/worker >/dev/null
  fi
  preview_k rollout status statefulset/worker --timeout=420s
}

session_preview_run() {
  local mode=$1
  shift
  node "$REPO_ROOT/e2e/k3d/session_preview_driver.mjs" "$NS" "$API_URL" "$mode" "$@"
}

session_preview_measure() {
  local variant=$1 round=$2 mode=${3:-load} started load_status=0
  local mark="$PREVIEW_EVIDENCE/$variant-$round"
  session_preview_run route
  preview_k get pods -l 'app in (coordinator,worker)' -o json >"$mark-pods.json"
  { uptime; cat /proc/pressure/cpu /proc/pressure/io; } >"$mark-host.txt"
  sample_capacity postgres-primary-0 >"$mark-capacity.ndjson" &
  CAPACITY_SAMPLER=$!
  preview_pg -c 'SELECT coalesce(sum(calls),0) FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database());' >"$mark-pg-before.txt"
  started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  session_preview_run "$mode" "$variant" "$round" || load_status=$?
  preview_pg -c 'SELECT coalesce(sum(calls),0) FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database());' >"$mark-pg-after.txt"
  kill "$CAPACITY_SAMPLER" 2>/dev/null || true
  wait "$CAPACITY_SAMPLER" 2>/dev/null || true
  CAPACITY_SAMPLER=""
  { uptime; cat /proc/pressure/cpu /proc/pressure/io; } >"$mark-host-after.txt"
  preview_pg -c 'SELECT usename, count(*) FROM pg_stat_activity WHERE datname = current_database() GROUP BY usename ORDER BY usename;' >"$mark-connections.txt"
  preview_k logs -l app=coordinator -c worker-tls --since-time="$started" --tail=-1 >"$mark-worker-http.log"
  if [[ "$load_status" != 0 ]]; then return "$load_status"; fi
  node --input-type=module - "$mark-worker-http.log" "$REPO_ROOT/e2e/k3d/session_preview_driver.mjs" <<'EOF' >"$mark-commit-http.json"
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const { tail } = await import(pathToFileURL(process.argv[3]));
const requests = [...readFileSync(process.argv[2], 'utf8').matchAll(/^POST \/v1\/worker\/commit-claimed HTTP\/[^ ]+ (\d+) .* ([\d.]+)$/gm)];
if (!requests.length) throw new Error('no authoritative Worker commit HTTP requests observed');
console.log(JSON.stringify({ successfulMs: tail(requests.filter(([,status]) => status === '200').map(([, , seconds]) => 1000 * Number(seconds))),
  otherStatus: requests.filter(([,status]) => status !== '200').length }));
EOF
  for pod in $(preview_k get pods -l 'app in (coordinator,worker)' -o name); do
    preview_k exec "$pod" -- sh -c 'cat /sys/fs/cgroup/memory.peak 2>/dev/null || cat /sys/fs/cgroup/memory/memory.max_usage_in_bytes' \
      >"$mark-${pod#pod/}-memory.txt"
  done
}

session_preview_validate() {
  export ADR71_AGENT_ID=adr71-session-preview
  node "$DRIVER" prepare-dispatch-load "$API_URL"
  session_preview_run cold
  preview_k scale deploy/session-preview-nats --replicas=1
  preview_k rollout status deploy/session-preview-nats --timeout=90s
  session_preview_run broker
  session_preview_run timeout
  session_preview_run burst
  session_preview_versions "$IMAGE" "$ADR71_PREVIEW_BASELINE_IMAGE"
  session_preview_run mixed old-worker
  session_preview_versions "$ADR71_PREVIEW_BASELINE_IMAGE" "$IMAGE"
  session_preview_run mixed old-coordinator
  preview_pg -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements;'
  local round variant image
  for round in 1 2; do
    local variants=(baseline candidate)
    if (( round == 2 )); then variants=(candidate baseline); fi
    for variant in "${variants[@]}"; do
      image="$IMAGE"
      if [[ "$variant" = baseline ]]; then image="$ADR71_PREVIEW_BASELINE_IMAGE"; fi
      session_preview_versions "$image" "$image" 1
      session_preview_measure "$variant" "$round"
    done
  done
  session_preview_versions "$IMAGE" "$IMAGE"
  session_preview_run long
  session_preview_measure candidate broker-outage load-fault
}
