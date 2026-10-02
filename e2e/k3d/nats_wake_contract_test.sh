#!/usr/bin/env bash
# Exercise the canonical runner's predicates and cleanup, without a live cluster.
set -euo pipefail

# This runner takes no arguments. Accepting one silently is how a caller's
# intent disappears into a run that answers a different question, and here it
# is minutes of real cluster or server work; refuse before any of it.
if [ "$#" -ne 0 ]; then
  echo "usage: e2e/k3d/nats_wake_contract_test.sh" >&2
  exit 2
fi
source "$(dirname "$0")/nats_wake_e2e.sh"
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
python3 "$REPO_ROOT/e2e/k3d/test_nats_wake_evidence.py"

# N1 invalid batch/port => explicit admission error before Docker, allocation
# or cluster work. Every other input is valid so another denial cannot mask it.
admissions=0
k3d_admit_or_exit() { admissions=$((admissions + 1)); return 99; }
for invalid in 0 -1 01 1.5 nope 1001 18446744073709551616; do
  status=0
  main "$invalid" >/dev/null 2>&1 || status=$?
  [[ "$status" = 2 && "$admissions" = 0 && -z "$RUN_DIR" ]]
done
for LOCAL_PORT in 0 80 65535 99999 nope; do
  status=0
  main 2 >/dev/null 2>&1 || status=$?
  [[ "$status" = 2 && "$admissions" = 0 && -z "$RUN_DIR" ]]
done
LOCAL_PORT=38651 M=2

# N2-N7 process status x exact receipt: only success+matching count admits.
# A valid-looking receipt cannot hide a failed/timed-out driver, and a zero
# exit cannot hide missing/wrong count. Call the actual submit_batch function.
run_with_deadline() {
  [[ "$*" = "30 env THREAD_PREFIX=fixture node $DRIVER submit http://127.0.0.1:$LOCAL_PORT 2" ]] || return 99
  printf '%s\n' "$fixture_response"
  return "$driver_status"
}
for mode in success warnings failure timeout wrong-count absent; do
  fixture_response='OK submitted=2' driver_status=0 expected=0
  case "$mode" in
    warnings) fixture_response=$'fixture warning\nOK submitted=2' ;;
    failure) driver_status=17; expected=17 ;;
    timeout) driver_status=124; expected=124 ;;
    wrong-count) fixture_response='OK submitted=1'; expected=1 ;;
    absent) fixture_response=''; expected=1 ;;
  esac
  status=0
  submit_batch fixture >/dev/null 2>&1 || status=$?
  [[ "$status" = "$expected" ]]
done

# N8-N14 one SQL snapshot x each independent count x command success: exact
# total/distinct/per-Thread counts AND successful observation are all required.
# A permanent unknown or wrong count reaches the bounded failure; a later exact
# snapshot makes progress. Actual PostgreSQL semantics remain in the live E2E.
run_with_deadline() {
  [[ "$1" =~ ^[1-9][0-9]*$ && "$*" = *"count(DISTINCT thread_id)"* && "$*" = *"ON_ERROR_STOP=1"* ]] || return 99
  head -1 "$TEST_ROOT/rows"
  if (( $(wc -l < "$TEST_ROOT/rows") > 1 )); then
    tail -n +2 "$TEST_ROOT/rows" > "$TEST_ROOT/rest"
    mv "$TEST_ROOT/rest" "$TEST_ROOT/rows"
  fi
  return "$query_status"
}
query_status=0
printf '4 2 0\n' > "$TEST_ROOT/rows"
wait_batch 2 1 >/dev/null
printf '0 0 0\n4 2 0\n' > "$TEST_ROOT/rows"
wait_batch 2 5 >/dev/null
for row in '3 2 0' '4 1 0' '4 2 1' '' '4 2 0 extra'; do
  printf '%s\n' "$row" > "$TEST_ROOT/rows"
  ! wait_batch 2 1 >/dev/null 2>&1
done
printf '4 2 0\n' > "$TEST_ROOT/rows"
query_status=23
! wait_batch 2 1 >/dev/null 2>&1

# N15 build failure has no cluster => no ambient diagnostic/deletion. N16 a
# create attempt fails => bounded exact-context diagnostics precede cleanup.
# N17 diagnostic failure cannot hide cluster-cleanup failure; owned local files
# are still removed. The shared EXIT owner separately proves status precedence.
calls=""
k3d_diagnostic_kubectl() { calls+=" diagnostic:$*"; return 29; }
k3d_delete_cluster() { calls+=" delete:$1"; return "$delete_status"; }
for CLUSTER_ATTEMPTED in 0 1; do
  RUN_DIR=$(mktemp -d "$TEST_ROOT/run.XXXXXX")
  CLUSTER=awaken-nats-fixture delete_status=31 calls=""
  touch "$RUN_DIR/owned"
  status=0
  cleanup 17 || status=$?
  [[ ! -e "$RUN_DIR" ]]
  if (( CLUSTER_ATTEMPTED == 0 )); then
    [[ "$status" = 0 && -z "$calls" ]]
  else
    [[ "$status" = 31 && "$calls" = " diagnostic:$CLUSTER "*" delete:$CLUSTER" ]]
  fi
done

# N23 owned-image cleanup failure must fail a successful body; cluster cleanup
# failure remains first. Both port-forward children are killed and reaped, and
# local files are removed in either case. Never remove a shared/base image.
IMAGE_BUILT=1 IMAGE=awaken-nats-fixture:local PF_PID=111 MONITOR_PID=222
run_with_deadline() {
  [[ "$*" = "$K3D_DIAGNOSTIC_TIMEOUT_SECONDS docker image rm $IMAGE" ]] || return 99
  calls+=" image:$IMAGE"; return 32
}
kill() { calls+=" kill:$*"; }
wait() { calls+=" wait:$*"; }
for delete_status in 0 31; do
  RUN_DIR=$(mktemp -d "$TEST_ROOT/image.XXXXXX")
  calls="" status=0
  cleanup 0 || status=$?
  [[ ! -e "$RUN_DIR" && "$calls" = " kill:111 wait:111 kill:222 wait:222 delete:$CLUSTER image:$IMAGE" ]]
  if (( delete_status == 0 )); then [[ "$status" = 32 ]]; else [[ "$status" = 31 ]]; fi
done
IMAGE_BUILT=0 PF_PID="" MONITOR_PID=""

# N18 two runs keep separate immutable deployment inputs. The actual Kustomize
# transformer replaces only the scenario image; Postgres/NATS and base files
# remain canonical. No cluster is contacted by `kubectl kustomize`.
for CLUSTER in awaken-nats-first awaken-nats-second; do
  RUN_DIR=$(mktemp -d "$TEST_ROOT/overlay.XXXXXX")
  render_overlay
  kubectl kustomize "$RUN_DIR" > "$RUN_DIR/rendered.yaml"
  grep -Fq "image: $CLUSTER:local" "$RUN_DIR/rendered.yaml"
  ! grep -Fq 'image: awaken-topology:' "$RUN_DIR/rendered.yaml"
  grep -Fq 'image: postgres:16' "$RUN_DIR/rendered.yaml"
  grep -Fq 'image: nats:2' "$RUN_DIR/rendered.yaml"
  grep -Fq 'containerPort: 8222' "$RUN_DIR/rendered.yaml"
done

# N19-N22 transport status x JSON validity x broker progress. Exercise the
# actual curl->file->parser composition: a nonzero HTTP command cannot admit
# even a valid body; an eventual observation may progress within the deadline.
MONITOR_PORT=42222
curl() {
  [[ "$*" = "--max-time 2 -fsS http://127.0.0.1:42222/varz" ]] || return 99
  cat "$TEST_ROOT/broker.json"
  return "$curl_status"
}
printf '%s\n' '{"server_id":"original","connections":1,"subscriptions":1,"in_msgs":10,"out_msgs":30}' > "$TEST_ROOT/broker.json"
curl_status=0
wait_nats '' 1 >/dev/null
cp "$RUN_DIR/nats-current.json" "$RUN_DIR/nats-before.json"
! wait_nats "$RUN_DIR/nats-before.json" 1 >/dev/null 2>&1
printf '%s\n' '{"server_id":"original","connections":1,"subscriptions":1,"in_msgs":11,"out_msgs":31}' > "$TEST_ROOT/broker.json"
wait_nats "$RUN_DIR/nats-before.json" 1 >/dev/null
curl_status=22
! wait_nats "$RUN_DIR/nats-before.json" 1 >/dev/null 2>&1
curl_status=0
printf 'broken' > "$TEST_ROOT/broker.json"
! wait_nats '' 1 >/dev/null 2>&1

# N24 execute the real main/EXIT composition in a fresh shell (testing a shell
# function under `!` would disable its errexit). A failed canonical snapshot
# must preserve status 27, never build an image or create/delete a cluster, and
# remove its private run directory. The resolver owns concurrent Cargo safety;
# this row locks the production runner onto that snapshot entrypoint.
status=0
bash -s -- "$REPO_ROOT" "$TEST_ROOT" > "$TEST_ROOT/main.log" 2>&1 <<'MAIN_PROBE' || status=$?
set -euo pipefail
source "$1/e2e/k3d/nats_wake_e2e.sh"
probe_root="$2"
k3d_admit_or_exit() { :; }
resolve_cargo_executable() {
  printf '%s\n' "$RUN_DIR" > "$probe_root/allocated"
  [[ "$*" = "awaken-scenario-host awaken-scenario-host --features nats --snapshot-destination $RUN_DIR/awaken-server" ]] || return 99
  return 27
}
k3d_docker_build() { touch "$probe_root/unexpected-build"; return 99; }
k3d_delete_cluster() { touch "$probe_root/unexpected-delete"; return 99; }
main 2
MAIN_PROBE
[[ "$status" = 27 && ! -e "$TEST_ROOT/unexpected-build" && ! -e "$TEST_ROOT/unexpected-delete" ]]
[[ ! -e "$(cat "$TEST_ROOT/allocated")" ]]
echo 'OK - NATS admission, receipt, SQL, isolation and cleanup rules hold.'
