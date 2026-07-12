#!/usr/bin/env bash
# Graceful shutdown / drain-on-SIGTERM for the durable server (awaken-server-local).
#
# Pins the shutdown contract of the durable ingress under a *real* SIGTERM — the
# signal an orchestrator (Kubernetes, systemd, `docker stop`) sends before the hard
# SIGKILL. This is a SHELL e2e (not a .mjs) on purpose: it needs to signal a spawned
# child process and observe its exit code, which an in-process Rust test cannot do.
#
# It proves two things:
#   1. FIX / regression guard — SIGTERM triggers graceful shutdown, so the process
#      exits cleanly (code 0) within a bounded time. Before the fix SIGTERM took the
#      default disposition: an immediate kill (exit 143) that dropped in-flight
#      foreground work and skipped the observability span flush.
#   2. Durability across the SIGTERM boundary — over the shared SQLite durable queue,
#      (a) a run committed before shutdown survives the restart committed EXACTLY
#      once (no loss, no double-commit), and (b) a run submitted and then caught by
#      an immediate SIGTERM is driven to completion EXACTLY once on the next start —
#      either it drained, or it was stranded claimed-but-not-settled and recovered
#      by the fresh pool after its lease expired. Never zero, never twice.
#
# Run: RUSTUP_TOOLCHAIN=1.96.0 bash e2e/graceful_drain_e2e.sh
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

: "${RUSTUP_TOOLCHAIN:=1.96.0}"
export RUSTUP_TOOLCHAIN
PORT="${E2E_PORT:-38798}"
BASE="http://127.0.0.1:${PORT}"
# SQLite durable backend under a tmp store dir — deterministic, no Postgres needed.
STORE_DIR="$(mktemp -d)"
# The dispatch lease (DEFAULT_LEASE_MS) is 30s: a run stranded mid-drive by the
# abrupt shutdown becomes reclaimable only after its lease expires, so the recovery
# wait must be generous.
RECOVERY_TIMEOUT=45

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null
  rm -rf "$STORE_DIR"
}
trap cleanup EXIT

fail() { echo "GRACEFUL DRAIN E2E FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*"; }

echo "==> building awaken-server-local (toolchain $RUSTUP_TOOLCHAIN)"
cargo build --quiet -p awaken-server-local --bin awaken-server-local \
  || fail "build failed"
TARGET_DIR="$(cargo metadata --format-version=1 --no-deps 2>/dev/null \
  | jq -r '.target_directory')"
BIN="${TARGET_DIR}/debug/awaken-server-local"
[ -x "$BIN" ] || fail "binary not found at $BIN"

start_server() {
  AWAKEN_HTTP_ADDR="127.0.0.1:${PORT}" \
  AWAKEN_MODEL_MODE=echo \
  AWAKEN_INGRESS=durable \
  AWAKEN_STORAGE_DIR="$STORE_DIR" \
    "$BIN" >"$STORE_DIR/server.log" 2>&1 &
  SERVER_PID=$!
}

wait_for_ready() {
  for _ in $(seq 1 100); do
    curl -sf "${BASE}/v1/durable/threads/readyprobe/messages" >/dev/null 2>&1 && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || fail "server died during startup; log:$(cat "$STORE_DIR/server.log")"
    sleep 0.1
  done
  fail "server did not become ready on $BASE"
}

submit_background() { # thread text
  curl -sf -X POST "${BASE}/v1/durable/threads/$1/submit_background" \
    -H 'content-type: application/json' -d "{\"text\":\"$2\"}" \
    || fail "submit_background($1) failed"
}

assistant_count() { # thread -> stdout: count of non-empty assistant replies
  curl -sf "${BASE}/v1/durable/threads/$1/messages" \
    | jq '[.messages[] | select(.role=="Assistant" and ((.text//"")|length>0))] | length'
}

wait_for_assistants() { # thread expected timeout_s
  local deadline=$(( $(date +%s) + $3 ))
  while :; do
    local n; n="$(assistant_count "$1")"
    [ "$n" -ge "$2" ] && return 0
    [ "$(date +%s)" -ge "$deadline" ] && fail "thread $1: timed out waiting for $2 assistant reply(ies); saw $n"
    sleep 0.2
  done
}

# Send SIGTERM and assert the process exits gracefully (code 0) within `bound` s.
sigterm_expect_graceful() { # bound_s
  kill -TERM "$SERVER_PID" || fail "kill -TERM failed"
  for _ in $(seq 1 $(( $1 * 10 )) ); do
    kill -0 "$SERVER_PID" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -9 "$SERVER_PID" 2>/dev/null
    fail "server did not exit within ${1}s of SIGTERM (no graceful drain)"
  fi
  wait "$SERVER_PID"; local code=$?
  SERVER_PID=""
  [ "$code" -eq 0 ] || fail "SIGTERM exit code $code (expected 0 graceful; 143 = hard-killed default disposition)"
}

echo "==> PART 1: SIGTERM triggers a graceful, bounded, clean exit"
start_server
wait_for_ready
sigterm_expect_graceful 10
pass "SIGTERM drained and exited 0 within bound (not 143)"

echo "==> PART 2: durability is exactly-once across the SIGTERM boundary"
start_server
wait_for_ready
# A run we let commit fully before shutdown.
submit_background committed "before shutdown" >/dev/null
wait_for_assistants committed 1 20
pass "thread 'committed' driven to a single reply before shutdown"

# A run submitted and then caught by an immediate SIGTERM — may drain, or may be
# stranded claimed-but-not-settled until its lease expires and a fresh pool recovers
# it. Either way it must land exactly once on restart.
submit_background inflight "at shutdown" >/dev/null
sigterm_expect_graceful 10
pass "SIGTERM with a fresh submission in flight still exited 0 gracefully"

echo "==> restart over the SAME durable store"
start_server
wait_for_ready

n_committed="$(assistant_count committed)"
[ "$n_committed" -eq 1 ] || fail "thread 'committed' has $n_committed replies after restart (expected exactly 1: no loss, no double-commit)"
pass "thread 'committed' survived the SIGTERM restart with exactly 1 reply"

# No run is lost: the in-flight run is driven (drained or lease-recovered) exactly once.
wait_for_assistants inflight 1 "$RECOVERY_TIMEOUT"
# Give any erroneous double-drive a window to also land, then assert it did NOT.
sleep 2
n_inflight="$(assistant_count inflight)"
[ "$n_inflight" -eq 1 ] || fail "thread 'inflight' has $n_inflight replies (expected exactly 1: not lost, not double-driven)"
pass "thread 'inflight' recovered to exactly 1 reply after the abrupt SIGTERM (no loss, no double-drive)"

# The pool still drains fresh work after the restart.
submit_background afterrestart "post restart" >/dev/null
wait_for_assistants afterrestart 1 20
pass "pool keeps draining fresh submissions after the restart"

sigterm_expect_graceful 10

echo
echo "GRACEFUL DRAIN E2E PASS: SIGTERM drains to a clean exit, and durable runs are"
echo "committed exactly once across the shutdown boundary (no loss, no double-drive)."
