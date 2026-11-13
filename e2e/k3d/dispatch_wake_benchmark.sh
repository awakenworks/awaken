#!/usr/bin/env bash
# Opt-in benchmark hook for distributed_control_e2e.sh. All workload goes
# through the public Managed API and the existing registered Worker fleet.
set -euo pipefail
if [[ "$#" -ne 2 || -z "$1" || "$1" == -* || "$2" != http://* && "$2" != https://* ]]; then
  echo 'usage: e2e/k3d/dispatch_wake_benchmark.sh NAMESPACE API_URL' >&2
  exit 2
fi
ns=$1 api=$2
evidence=${ADR71_WAKE_EVIDENCE_DIR:?set an output directory for benchmark evidence}
rounds=${ADR71_WAKE_ROUNDS:-3}
export ADR71_LOAD_REQUESTS=${ADR71_LOAD_REQUESTS:-1200}
export ADR71_LOAD_CONCURRENCY=${ADR71_LOAD_CONCURRENCY:-48}
export ADR71_LOAD_ROUNDS=1
export ADR71_WAKE_REQUIRE_SATURATION=${ADR71_WAKE_REQUIRE_SATURATION:-1}
completion_probe=${ADR71_WAKE_COMPLETION_PROBE:-0}
if [[ "$completion_probe" != 0 && "$completion_probe" != 1 ]]; then
  echo 'ADR71_WAKE_COMPLETION_PROBE must be 0 or 1' >&2
  exit 2
fi
profile=${ADR71_WAKE_PROFILE:-matrix}
case "$profile" in
  matrix) modes=(default pg-notify nats) ;;
  default) modes=(default) ;;
  legacy-outage) modes=() ;;
  *) echo 'invalid ADR71_WAKE_PROFILE' >&2; exit 2 ;;
esac
if ! [[ "$rounds" =~ ^[1-9][0-9]?$ ]] || (( rounds > 12 )); then
  echo 'ADR71_WAKE_ROUNDS must be between 1 and 12' >&2
  exit 2
fi
mkdir -p "$evidence"
summary="$evidence/summary.tsv"
if [[ -e "$summary" ]]; then
  echo "evidence already exists: $summary" >&2
  exit 2
fi
printf 'round\tmode\tfault\tadmitted\tclaimed\tp99_ms\tp999_ms\tpeak_pending\tsql_calls\tnotify_calls\tdispatch_sql_calls\tload_status\tvalid\tfull_seconds\tworker_peaks\tfull_samples\tfull_p99_ms\tfull_p999_ms\tlongest_full_seconds\tworker_slot_utilization_pct\twall_seconds\n' >"$summary"
k() { kubectl -n "$ns" "$@"; }
pg() { k exec -i postgres-primary-0 -- psql -U postgres -d runtime -v ON_ERROR_STOP=1 -At "$@"; }
load_command=(node)
if [[ -n "${ADR71_WAKE_LOAD_CPUS:-}" ]]; then
  load_command=(taskset -c "$ADR71_WAKE_LOAD_CPUS" node)
fi
k get deploy control coordinator provider awaken-api -o wide >"$evidence/deployments.txt"
k get statefulset worker postgres-primary postgres-standby -o wide >"$evidence/statefulsets.txt"
export ADR71_AGENT_ID=adr71-dispatch-benchmark
"${load_command[@]}" e2e/k3d/distributed_control_driver.ts prepare-dispatch-load "$api"

cat <<'YAML' | k apply -f -
apiVersion: apps/v1
kind: Deployment
metadata: {name: dispatch-benchmark-nats}
spec:
  replicas: 1
  selector: {matchLabels: {app: dispatch-benchmark-nats}}
  template:
    metadata: {labels: {app: dispatch-benchmark-nats}}
    spec:
      containers:
        - name: nats
          image: nats:2
          imagePullPolicy: Never
          args: ["-D"]
          ports: [{containerPort: 4222}]
          readinessProbe: {tcpSocket: {port: 4222}}
          resources:
            requests: {cpu: 100m, memory: 32Mi}
            limits: {cpu: "1", memory: 128Mi}
---
apiVersion: v1
kind: Service
metadata: {name: dispatch-benchmark-nats}
spec:
  selector: {app: dispatch-benchmark-nats}
  ports: [{port: 4222, targetPort: 4222}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: dispatch-benchmark-nats}
spec:
  podSelector: {matchLabels: {app: coordinator}}
  policyTypes: [Egress]
  egress:
    - to: [{podSelector: {matchLabels: {app: dispatch-benchmark-nats}}}]
      ports: [{protocol: TCP, port: 4222}]
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: dispatch-benchmark-nats-ingress}
spec:
  podSelector: {matchLabels: {app: dispatch-benchmark-nats}}
  policyTypes: [Ingress]
  ingress:
    - from: [{podSelector: {matchLabels: {app: coordinator}}}]
      ports: [{protocol: TCP, port: 4222}]
YAML
k rollout status deploy/dispatch-benchmark-nats --timeout=90s
worker_capacity=$(k get secret adr71-role-config -o jsonpath='{.data.worker\.toml}' | base64 -d |
  awk '/^worker_max_concurrent = / {print $3}')
[[ "$worker_capacity" =~ ^[1-9][0-9]*$ ]]

pg <<'SQL'
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
CREATE TABLE IF NOT EXISTS dispatch_wake_measurement (
    run_id text NOT NULL,
    phase text NOT NULL,
    xid xid NOT NULL,
    PRIMARY KEY (run_id, phase)
);
CREATE TABLE IF NOT EXISTS dispatch_wake_attempt (
    run_id text NOT NULL,
    attempt_owner text NOT NULL,
    attempt_epoch bigint NOT NULL,
    occupied boolean NOT NULL,
    xid xid NOT NULL
);
CREATE OR REPLACE FUNCTION observe_dispatch_wake_measurement() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND (OLD.active_attempt_owner, OLD.active_attempt_epoch)
        IS DISTINCT FROM (NEW.active_attempt_owner, NEW.active_attempt_epoch) THEN
        IF OLD.active_attempt_owner IS NOT NULL THEN
            INSERT INTO dispatch_wake_attempt VALUES
                (OLD.run_id, OLD.active_attempt_owner, OLD.active_attempt_epoch, false, pg_current_xact_id()::xid);
        END IF;
        IF NEW.active_attempt_owner IS NOT NULL THEN
            INSERT INTO dispatch_wake_attempt VALUES
                (NEW.run_id, NEW.active_attempt_owner, NEW.active_attempt_epoch, true, pg_current_xact_id()::xid);
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        IF OLD.active_attempt_owner IS NOT NULL THEN
            INSERT INTO dispatch_wake_attempt VALUES
                (OLD.run_id, OLD.active_attempt_owner, OLD.active_attempt_epoch, false, pg_current_xact_id()::xid);
        END IF;
        RETURN OLD;
    END IF;
    IF TG_OP = 'INSERT' AND NEW.status = 'pending' THEN
        INSERT INTO dispatch_wake_measurement VALUES (NEW.run_id, 'enqueue', pg_current_xact_id()::xid)
        ON CONFLICT DO NOTHING;
    ELSIF TG_OP = 'UPDATE' AND OLD.status = 'reserved' AND NEW.status = 'pending' THEN
        INSERT INTO dispatch_wake_measurement VALUES (NEW.run_id, 'enqueue', pg_current_xact_id()::xid)
        ON CONFLICT DO NOTHING;
    ELSIF NEW.status = 'running' AND OLD.status <> 'running' THEN
        INSERT INTO dispatch_wake_measurement VALUES (NEW.run_id, 'claim', pg_current_xact_id()::xid)
        ON CONFLICT DO NOTHING;
    END IF;
    RETURN NEW;
END $$;
CREATE OR REPLACE TRIGGER dispatch_wake_measurement AFTER INSERT OR UPDATE OR DELETE ON runtime_dispatch
FOR EACH ROW EXECUTE FUNCTION observe_dispatch_wake_measurement();
TRUNCATE dispatch_wake_measurement, dispatch_wake_attempt;
SQL

base_config=$(k get secret adr71-role-config -o jsonpath='{.data.coordinator\.toml}' | base64 -d)
set_mode() {
  local mode=$1
  printf '%s' "$base_config" | python3 -c '
import json, re, sys
config = sys.stdin.read()
mode = sys.argv[1]
config = re.sub(r"^(dispatch_wake|nats_url|postgres_min_connections)[ \t]*=.*\n", "", config, flags=re.M)
pool = re.search(r"^postgres_max_connections = (\d+)$", config, re.M)
assert pool
settings = [f"postgres_min_connections = {pool[1]}"]
if mode != "default":
    settings.append(f"dispatch_wake = \"{mode}\"")
if mode == "nats":
    settings.append("nats_url = \"nats://dispatch-benchmark-nats:4222\"")
config = "\n".join(settings) + "\n" + config
print(json.dumps({"stringData": {"coordinator.toml": config}}))
' "$mode" | k patch secret adr71-role-config --type=merge --patch-file /dev/stdin
  k rollout restart deploy/coordinator
  k rollout status deploy/coordinator --timeout=180s
  # Pod readiness can precede propagation through the public Service. Probe
  # the Coordinator route with a read before creating any benchmark Sessions.
  local status terminating_pods consecutive=0
  for _ in {1..30}; do
    terminating_pods=$(k get pod -l app=coordinator -o jsonpath='{range .items[?(@.metadata.deletionTimestamp)]}{.metadata.name}{"\n"}{end}')
    status=$(curl -s --max-time 3 -o /dev/null -w '%{http_code}' \
      -H "x-api-key: ${ADR71_API_KEY:-e2e-dummy}" -H 'anthropic-version: 2023-06-01' \
      -H 'anthropic-beta: managed-agents-2026-04-01' "$api/v1/sessions?limit=1") || status=000
    if [[ "$status" == 200 && -z "$terminating_pods" ]]; then
      (( consecutive += 1 ))
      (( consecutive < 3 )) || break
    else
      consecutive=0
    fi
    sleep 1
  done
  (( consecutive >= 3 ))
  [[ "$(pg -c "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND query LIKE 'LISTEN %'")" == 0 ]]
  if [[ "$mode" != default ]]; then
    local pod
    for pod in $(k get pod -l app=coordinator -o name); do
      k logs "$pod" -c coordinator | grep -F 'ignored without a local Coordinator claim pool' || return 1
    done
  fi
}

sql_calls() {
  pg -c "SELECT coalesce(sum(calls),0),
                coalesce(sum(calls) FILTER (WHERE query ILIKE '%pg_notify%'),0),
                coalesce(sum(calls) FILTER (WHERE query ILIKE '%runtime_dispatch%'),0),
                (SELECT dealloc FROM pg_stat_statements_info)
         FROM pg_stat_statements WHERE toplevel AND dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
         AND userid = (SELECT oid FROM pg_roles WHERE rolname = 'awaken_coordinator')
         AND query NOT ILIKE '%pg_stat_statements%';"
}

db_connections() {
  pg -c "SELECT count(*) FILTER (WHERE state IS DISTINCT FROM 'idle' OR query NOT LIKE 'LISTEN %'),
                count(*) FILTER (WHERE state = 'idle' AND query LIKE 'LISTEN %')
         FROM pg_stat_activity WHERE datname = current_database() AND usename = 'awaken_coordinator';"
}

assert_no_nats_clients() {
  local destination=$1 pod_ips=$2 ip broker_logs
  [[ -n "$pod_ips" ]]
  broker_logs=$(k logs deploy/dispatch-benchmark-nats)
  printf '%s\n' "$broker_logs" >"$destination"
  while IFS= read -r ip; do
    if grep -F "$ip:" <<<"$broker_logs" | grep -F 'Client connection created' >/dev/null; then
      echo 'split Coordinator opened a NATS connection' >&2
      return 1
    fi
  done <<<"$pod_ips"
}

fault_pid=
broker_stopped=0
cleanup_fault() {
  if [[ -n "$fault_pid" ]]; then
    kill "$fault_pid" 2>/dev/null || true
    wait "$fault_pid" 2>/dev/null || true
  fi
  if (( broker_stopped )); then
    k scale deploy/dispatch-benchmark-nats --replicas=1 >/dev/null || true
  fi
}
trap cleanup_fault EXIT
matrix_status=0
measure() {
  local round=$1 mode=$2 fault=$3 prefix="$evidence/$1-$2-$3"
  [[ ! -e "$prefix.started" ]] || { echo "evidence already exists: $prefix" >&2; return 1; }
  local broker_pod
  if [[ "$fault" == outage ]]; then
    broker_pod=$(k get pod -l app=dispatch-benchmark-nats -o name)
    [[ "$broker_pod" == pod/* && "$broker_pod" != *$'\n'* ]]
    broker_stopped=1
    k scale deploy/dispatch-benchmark-nats --replicas=0
    k wait --for=delete "$broker_pod" --timeout=60s
    date -u +%Y-%m-%dT%H:%M:%S.%NZ >"$prefix.broker-down"
  fi
  set_mode "$mode" >"$prefix.startup" 2>&1
  cat "$prefix.startup"
  local coordinator_pods= coordinator_ips=
  if [[ "$mode" == nats ]]; then
    coordinator_pods=$(k get pod -l app=coordinator -o jsonpath='{range .items[*]}{.metadata.uid}{"\n"}{end}')
    coordinator_ips=$(k get pod -l app=coordinator -o jsonpath='{range .items[*]}{.status.podIP}{"\n"}{end}')
    if [[ "$fault" == normal ]]; then
      assert_no_nats_clients "$prefix.nats-before" "$coordinator_ips"
    fi
  fi
  k get pod -o wide >"$prefix.pods-before"
  k get pod \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{range .spec.containers[*]}{.name}{"="}{.resources}{" "}{end}{"\n"}{end}' \
    >"$prefix.resources"
  uptime >"$prefix.host"
  cat /proc/pressure/{cpu,io,memory} >>"$prefix.host"
  export ADR71_WAKE_LOAD_MARKER="$prefix.started"
  (
    until [[ -f "$ADR71_WAKE_LOAD_MARKER.warm" ]]; do sleep 0.2; done
    db_connections >"$prefix.connections-before"
    sql_calls >"$prefix.calls-before"
    touch "$ADR71_WAKE_LOAD_MARKER.counted"
    until [[ -f "$ADR71_WAKE_LOAD_MARKER.done" ]]; do sleep 0.2; done
    sql_calls >"$prefix.calls-after"
    db_connections >"$prefix.connections-after"
    touch "$ADR71_WAKE_LOAD_MARKER.done-counted"
  ) &
  local sql_probe=$!
  if [[ "$fault" == outage ]]; then
    (
      until [[ -f "$ADR71_WAKE_LOAD_MARKER" ]]; do sleep 1; done
      sleep "${ADR71_WAKE_OUTAGE_SECONDS:-45}"
      k scale deploy/dispatch-benchmark-nats --replicas=1
      k rollout status deploy/dispatch-benchmark-nats --timeout=90s
      date -u +%Y-%m-%dT%H:%M:%S.%NZ >"$prefix.broker-ready"
      [[ "$coordinator_pods" == "$(k get pod -l app=coordinator -o jsonpath='{range .items[*]}{.metadata.uid}{"\n"}{end}')" ]]
    ) >"$prefix.fault" 2>&1 &
    fault_pid=$!
  fi
  local load_status=0
  ADR71_LOAD_REPORT_FILE="$prefix.load.json" "${load_command[@]}" e2e/k3d/distributed_control_driver.ts load "$api" >"$prefix.log" 2>&1 || load_status=$?
  if (( load_status != 0 )); then
    printf 'Dispatch load exited with status %s. See %s\n' "$load_status" "$prefix.log" >&2
  fi
  if [[ ! -e "$ADR71_WAKE_LOAD_MARKER.done-counted" ]]; then
    kill "$sql_probe" 2>/dev/null || true
    wait "$sql_probe" 2>/dev/null || true
    tail -12 "$prefix.log" >&2
    return 1
  fi
  wait "$sql_probe"
  local fault_status=0
  if [[ -n "$fault_pid" ]]; then
    wait "$fault_pid" || fault_status=$?
    if (( fault_status != 0 )); then
      cleanup_fault
    fi
    fault_pid=
    broker_stopped=0
  fi
  unset ADR71_WAKE_LOAD_MARKER
  if [[ "$mode" == nats && "$fault_status" == 0 ]]; then
    assert_no_nats_clients "$prefix.nats-after" "$coordinator_ips"
  fi
  local started sessions
  started=$(cat "$prefix.started")
  sessions=$(cat "$prefix.started.sessions")
  pg -v started="$started" -v sessions="$sessions" >"$prefix.claims" <<'SQL'
SELECT e.run_id, pg_xact_commit_timestamp(e.xid), pg_xact_commit_timestamp(c.xid),
       1000 * extract(epoch FROM pg_xact_commit_timestamp(c.xid)-pg_xact_commit_timestamp(e.xid))
FROM dispatch_wake_measurement e LEFT JOIN dispatch_wake_measurement c
  ON c.run_id=e.run_id AND c.phase='claim'
JOIN runtime_dispatch_session_membership m ON m.run_id=e.run_id
WHERE e.phase='enqueue' AND pg_xact_commit_timestamp(e.xid) >= :'started'::timestamptz
  AND m.session_id IN (SELECT jsonb_array_elements_text(:'sessions'::jsonb))
ORDER BY pg_xact_commit_timestamp(e.xid), e.run_id;
SQL
  pg -v started="$started" -v sessions="$sessions" >"$prefix.attempts" <<'SQL'
SELECT a.attempt_owner, a.occupied::int, pg_xact_commit_timestamp(a.xid)
FROM dispatch_wake_attempt a
JOIN runtime_dispatch_session_membership m ON m.run_id=a.run_id
WHERE pg_xact_commit_timestamp(a.xid) >= :'started'::timestamptz
  AND m.session_id IN (SELECT jsonb_array_elements_text(:'sessions'::jsonb));
SQL
  python3 - "$prefix" "$worker_capacity" >"$prefix.capacity" <<'PY'
from bisect import bisect_right
from collections import Counter, defaultdict
from datetime import datetime
from decimal import Decimal
from math import ceil
from pathlib import Path
import sys

changes = defaultdict(Counter)
prefix = sys.argv[1]
capacity = int(sys.argv[2])
for line in Path(prefix + ".attempts").read_text().splitlines():
    owner, occupied, timestamp = line.split("|")
    changes[datetime.fromisoformat(timestamp)][owner] += 1 if occupied == "1" else -1

current = Counter()
peak = Counter()
full_seconds = 0.0
full_starts, full_ends = [], []
slot_seconds = Counter()
previous = None
for timestamp, delta in sorted(changes.items()):
    if previous is not None:
        elapsed = (timestamp - previous).total_seconds()
        for owner, count in current.items():
            slot_seconds[owner] += min(capacity, count) * elapsed
        if len(current) == 2 and all(count == capacity for count in current.values()):
            full_seconds += elapsed
            if full_ends and full_ends[-1] == previous:
                full_ends[-1] = timestamp
            else:
                full_starts.append(previous)
                full_ends.append(timestamp)
    current.update(delta)
    for owner, count in current.items():
        peak[owner] = max(peak[owner], count)
    previous = timestamp
saturated = []
for line in Path(prefix + ".claims").read_text().splitlines():
    _, enqueue, _, gap = line.split("|")
    stamp = datetime.fromisoformat(enqueue)
    interval = bisect_right(full_starts, stamp) - 1
    if gap and interval >= 0 and stamp < full_ends[interval]:
        saturated.append(Decimal(gap))
saturated.sort()
p99 = saturated[ceil(len(saturated) * .99) - 1] if len(saturated) >= 100 else ""
p999 = saturated[ceil(len(saturated) * .999) - 1] if len(saturated) >= 1000 else ""
longest = max(((end - start).total_seconds() for start, end in zip(full_starts, full_ends)), default=0)
elapsed = (max(changes) - min(changes)).total_seconds() if changes else 0
utilization = ",".join(f"{100 * seconds / (capacity * elapsed):.2f}"
                       for _, seconds in sorted(slot_seconds.items())) if elapsed else ""
print(f"{full_seconds:.3f}", ",".join(str(value) for value in sorted(peak.values())),
      len(saturated), p99, p999, f"{longest:.3f}", utilization, f"{elapsed:.3f}", sep="|")
PY
  python3 - "$prefix.claims" >"$prefix.latency" <<'PY'
from collections import defaultdict
from datetime import datetime
from decimal import Decimal
from math import ceil
from pathlib import Path
import sys

rows = [line.split("|") for line in Path(sys.argv[1]).read_text().splitlines()]
latencies = sorted(Decimal(gap) for _, _, _, gap in rows if gap and Decimal(gap) >= 0)
deltas = defaultdict(int)
for _, enqueue, claim, _ in rows:
    deltas[datetime.fromisoformat(enqueue)] += 1
    if claim:
        deltas[datetime.fromisoformat(claim)] -= 1
pending = peak_pending = 0
for stamp in sorted(deltas):
    pending += deltas[stamp]
    peak_pending = max(peak_pending, pending)

def percentile(fraction, minimum):
    if len(latencies) < minimum:
        return ""
    return latencies[ceil(len(latencies) * fraction) - 1]

print(len(rows), len(latencies), percentile(.99, 100), percentile(.999, 1000), peak_pending, sep="|")
PY
  local admitted claimed p99 p999 peak_pending
  IFS='|' read -r admitted claimed p99 p999 peak_pending <"$prefix.latency"
  local valid=1
  [[ "$admitted" == "$claimed" && "$claimed" -gt 0 ]] || valid=0
  if [[ -z "${ADR71_LOAD_RATE_PER_SECOND:-}" && "$admitted" != "$ADR71_LOAD_REQUESTS" ]]; then
    valid=0
  fi
  if [[ "$fault" == outage ]]; then
    if (( fault_status != 0 )); then
      valid=0
    else
      python3 - "$prefix" >"$prefix.outage-claims" <<'PY'
from datetime import datetime
from pathlib import Path
import sys

prefix = sys.argv[1]
down, ready = (datetime.fromisoformat(Path(prefix + suffix).read_text().strip())
               for suffix in (".broker-down", ".broker-ready"))
claims = [datetime.fromisoformat(claim)
          for _, _, claim, _ in (row.split("|") for row in Path(prefix + ".claims").read_text().splitlines())
          if claim]
print(sum(down <= claim < ready for claim in claims), sum(claim >= ready for claim in claims), sep="|")
PY
      local during recovered
      IFS='|' read -r during recovered <"$prefix.outage-claims"
      (( during > 0 && recovered > 0 )) || valid=0
    fi
  fi
  local full_seconds worker_peaks saturated_samples saturated_p99 saturated_p999 longest_full utilization wall_seconds
  IFS='|' read -r full_seconds worker_peaks saturated_samples saturated_p99 saturated_p999 longest_full utilization wall_seconds <"$prefix.capacity"
  if [[ "$ADR71_WAKE_REQUIRE_SATURATION" == 1 ]]; then
    if [[ "${longest_full%.*}" -lt 10 || ( "$completion_probe" == 0 && -z "$p999" ) ]]; then
      echo "Insufficient full-capacity evidence in $prefix (longest=$longest_full, total=$full_seconds, peaks=$worker_peaks, samples=$saturated_samples)" >&2
      valid=0
    fi
  fi
  pg -c 'TRUNCATE dispatch_wake_measurement, dispatch_wake_attempt' >/dev/null
  k get pod -o wide >"$prefix.pods-after"
  uptime >>"$prefix.host"
  cat /proc/pressure/{cpu,io,memory} >>"$prefix.host"
  local before_total before_notify before_dispatch before_evictions after_total after_notify after_dispatch after_evictions
  IFS='|' read -r before_total before_notify before_dispatch before_evictions <"$prefix.calls-before"
  IFS='|' read -r after_total after_notify after_dispatch after_evictions <"$prefix.calls-after"
  if [[ "$before_evictions" != "$after_evictions" ]] || (( after_total < before_total )); then
    echo "pg_stat_statements counters changed discontinuously in $prefix" >&2
    valid=0
  fi
  local pool_capacity replicas expected_connections
  pool_capacity=$(printf '%s\n' "$base_config" | awk '/^postgres_max_connections = / {print $3}')
  replicas=$(k get deploy coordinator -o jsonpath='{.spec.replicas}')
  # Each replica owns its authority and dependent-read pools.
  expected_connections=$((pool_capacity * 2 * replicas))
  if [[ "$(cat "$prefix.connections-before")" != "$expected_connections|0" || "$(cat "$prefix.connections-after")" != "$expected_connections|0" ]]; then
    echo "expected $expected_connections business connections and no LISTEN connections in $prefix" >&2
    valid=0
  fi
  local sql_total notify_total dispatch_total
  sql_total=$((after_total - before_total))
  notify_total=$((after_notify - before_notify))
  dispatch_total=$((after_dispatch - before_dispatch))
  (( notify_total == 0 )) || valid=0
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$round" "$mode" "$fault" "$admitted" "$claimed" "$p99" "$p999" "$peak_pending" \
    "$sql_total" "$notify_total" "$dispatch_total" "$load_status" "$valid" \
    "$full_seconds" "$worker_peaks" "$saturated_samples" "$saturated_p99" "$saturated_p999" \
    "$longest_full" "$utilization" "$wall_seconds" >>"$summary"
  printf 'DISPATCH_WAKE round=%s mode=%s fault=%s admitted=%s claimed=%s p99_ms=%s p999_ms=%s peak_pending=%s sql_calls=%s notify_calls=%s dispatch_sql_calls=%s load_status=%s valid=%s\n' \
    "$round" "$mode" "$fault" "$admitted" "$claimed" "$p99" "$p999" \
    "$peak_pending" "$sql_total" "$notify_total" "$dispatch_total" "$load_status" "$valid"
  if (( load_status != 0 || ! valid )); then
    matrix_status=1
  fi
}

for ((round=1; round<=rounds; round++)); do
  for ((position=0; position<${#modes[@]}; position++)); do
    measure "$round" "${modes[$(((position + round - 1) % ${#modes[@]}))]}" normal
  done
  if [[ "$profile" != default ]]; then
    measure "$round" nats outage
  fi
done
exit "$matrix_status"
