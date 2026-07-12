#!/usr/bin/env bash
# Config-reload & rolling-upgrade contract for the durable server (awaken-server-local).
#
# This is the §9 operational sliver: how does a durable server pick up a config
# change, and what happens to durable work across the process swap that a rolling
# upgrade is made of? It is a SHELL e2e (not a .mjs) on purpose: it must spawn,
# signal, and re-spawn child processes and read their exit codes — an in-process
# Rust test cannot.
#
# FINDING (pinned below, PART 1 & 2): there is NO live config reload.
#   * awaken-server-local reads ALL runtime config from environment variables ONCE,
#     in `main()`, before it binds the socket (model mode, ingress, storage dir,
#     dispatch/commit backends, listen address). It registers handlers for exactly
#     two signals — SIGINT and SIGTERM — and both mean the same thing: begin a
#     graceful shutdown. There is no SIGHUP handler, no config-file watch, no
#     inotify/notify, and no admin "reload" endpoint. Runtime config is therefore
#     FIXED AT STARTUP; the ONLY way to apply a config change is a full restart.
#   * PART 1 asserts this statically (the source carries no reload wiring).
#   * PART 2 asserts it at runtime: SIGHUP — the conventional "reload" signal — is
#     not wired, so it takes the default disposition (terminate). A running server
#     cannot be told to reload; you restart it.
#
# CONTRACT (pinned below, PART 3): the restart — the atomic unit of a rolling
# upgrade — is safe over the durable on-disk store. We simulate the upgrade of a
# single fleet member: stop the old process with SIGTERM (what an orchestrator
# sends), start a fresh process over the SAME durable store but with a CHANGED
# config (a different listen address — observable proof the new config is applied
# only on restart, never live), and assert that every durable run submitted to the
# old process completes EXACTLY ONCE on the new one (no loss, no duplication) and
# that the upgraded server serves again. This is the single-repo proxy for a
# mixed-version fleet: you cannot easily run two binary versions here, so we pin the
# restart-continuity contract that a rolling upgrade depends on.
#
# Run: RUSTUP_TOOLCHAIN=1.96.0 bash e2e/config_reload_e2e.sh

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

: "${RUSTUP_TOOLCHAIN:=1.96.0}"
export RUSTUP_TOOLCHAIN

# The rolling upgrade changes the listen address across the restart — an observable
# config change (config is per-process, read at startup). PORT_V1 is the "old
# version"; PORT_V2 is the "new version" after the config-changing restart.
PORT_V1="${E2E_PORT:-38820}"
PORT_V2="${E2E_PORT_V2:-38821}"
BASE_V1="http://127.0.0.1:${PORT_V1}"
BASE_V2="http://127.0.0.1:${PORT_V2}"

# SQLite durable backend under a tmp store dir — deterministic, no Postgres needed.
# The store dir is the ONE thing held constant across the config-changing restart:
# durable state is store-scoped, not process-scoped.
STORE_DIR="$(mktemp -d)"
# The dispatch lease (DEFAULT_LEASE_MS) is 30s: a run stranded mid-drive by an
# abrupt stop becomes reclaimable only after its lease expires, so the recovery
# wait must be generous.
RECOVERY_TIMEOUT=45

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null
  rm -rf "$STORE_DIR"
}
trap cleanup EXIT

fail() { echo "CONFIG RELOAD E2E FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*"; }

# ---------------------------------------------------------------------------
# PART 1 — STATIC: the server carries no live-reload wiring.
# ---------------------------------------------------------------------------
echo "==> PART 1: source carries no live config-reload wiring (config is startup-fixed)"
SRC="crates/bin/awaken-server-local/src"
# SIGHUP is the conventional reload signal; a config watcher would use notify/inotify
# or a file watch; a reload endpoint would route /reload. None must exist.
if grep -rniE "sighup|hangup|config.?reload|reload.?config|notify::recommended|inotify|/reload" "$SRC" >/dev/null 2>&1; then
  echo "    unexpected reload wiring:"; grep -rniE "sighup|hangup|config.?reload|reload.?config|notify::recommended|inotify|/reload" "$SRC"
  fail "found reload wiring in $SRC — the no-live-reload finding no longer holds; update this test"
fi
pass "no SIGHUP handler / no config-file watch / no /reload endpoint in $SRC"
# And the ONLY signals the binary handles are SIGINT + SIGTERM (both = graceful stop).
grep -qE "SignalKind::terminate" "$SRC/main.rs" || fail "expected SIGTERM handler in main.rs"
grep -qE "ctrl_c" "$SRC/main.rs"                 || fail "expected SIGINT (ctrl_c) handler in main.rs"
pass "the only wired signals are SIGINT + SIGTERM, both meaning graceful shutdown"

echo "==> building awaken-server-local (toolchain $RUSTUP_TOOLCHAIN)"
cargo build --quiet -p awaken-server-local --bin awaken-server-local \
  || fail "build failed"
TARGET_DIR="$(cargo metadata --format-version=1 --no-deps 2>/dev/null | jq -r '.target_directory')"
BIN="${TARGET_DIR}/debug/awaken-server-local"
[ -x "$BIN" ] || fail "binary not found at $BIN"

# start_server <port>
start_server() {
  AWAKEN_HTTP_ADDR="127.0.0.1:$1" \
  AWAKEN_MODEL_MODE=echo \
  AWAKEN_INGRESS=durable \
  AWAKEN_STORAGE_DIR="$STORE_DIR" \
    "$BIN" >"$STORE_DIR/server.log" 2>&1 &
  SERVER_PID=$!
}

# wait_for_ready <base>
wait_for_ready() {
  for _ in $(seq 1 100); do
    curl -sf "$1/v1/durable/threads/readyprobe/messages" >/dev/null 2>&1 && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || fail "server died during startup; log:$(cat "$STORE_DIR/server.log")"
    sleep 0.1
  done
  fail "server did not become ready on $1"
}

# submit_background <base> <thread> <text>
submit_background() {
  curl -sf -X POST "$1/v1/durable/threads/$2/submit_background" \
    -H 'content-type: application/json' -d "{\"text\":\"$3\"}" \
    || fail "submit_background($2) failed"
}

# assistant_count <base> <thread> -> count of non-empty assistant replies
assistant_count() {
  curl -sf "$1/v1/durable/threads/$2/messages" \
    | jq '[.messages[] | select(.role=="Assistant" and ((.text//"")|length>0))] | length'
}

# wait_for_assistants <base> <thread> <expected> <timeout_s>
wait_for_assistants() {
  local deadline=$(( $(date +%s) + $4 ))
  while :; do
    local n; n="$(assistant_count "$1" "$2")"
    [ "$n" -ge "$3" ] && return 0
    [ "$(date +%s)" -ge "$deadline" ] && fail "thread $2: timed out waiting for $3 assistant reply(ies); saw $n"
    sleep 0.2
  done
}

# sigterm_expect_graceful <bound_s> — SIGTERM and assert clean exit 0 within bound.
sigterm_expect_graceful() {
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
  [ "$code" -eq 0 ] || fail "SIGTERM exit code $code (expected 0 graceful)"
}

# ---------------------------------------------------------------------------
# PART 2 — RUNTIME: SIGHUP is not a reload signal (config cannot be reloaded live).
# ---------------------------------------------------------------------------
echo "==> PART 2: SIGHUP is not wired for reload — a running server can't be told to reload"
start_server "$PORT_V1"
wait_for_ready "$BASE_V1"
kill -HUP "$SERVER_PID" || fail "kill -HUP failed"
# With no SIGHUP handler registered, the signal takes its default disposition
# (terminate). A reload handler would instead keep the process alive and serving.
for _ in $(seq 1 50); do
  kill -0 "$SERVER_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$SERVER_PID" 2>/dev/null; then
  kill -9 "$SERVER_PID" 2>/dev/null; SERVER_PID=""
  fail "server survived SIGHUP — a reload handler exists that this test did not expect"
fi
wait "$SERVER_PID" 2>/dev/null; SERVER_PID=""
# It is really gone: the port no longer serves.
curl -sf "$BASE_V1/v1/durable/threads/readyprobe/messages" >/dev/null 2>&1 \
  && fail "port $PORT_V1 still serving after SIGHUP"
pass "SIGHUP took the default disposition (terminate); no live reload — you must restart"

# ---------------------------------------------------------------------------
# PART 3 — CONTRACT: rolling-upgrade restart is exactly-once over the durable store,
# and applies the NEW config (a changed listen address) only on restart.
# ---------------------------------------------------------------------------
echo "==> PART 3: rolling-upgrade restart — CONFIG CHANGE + durable exactly-once continuity"
echo "    (old version on :$PORT_V1  ->  new version on :$PORT_V2, SAME durable store)"

# --- old version (v1) on PORT_V1 ---
start_server "$PORT_V1"
wait_for_ready "$BASE_V1"

# A run we let commit fully before the upgrade stop.
submit_background "$BASE_V1" committed "before upgrade" >/dev/null
wait_for_assistants "$BASE_V1" committed 1 20
pass "thread 'committed' driven to a single reply on the old version"

# A run submitted and then caught by the graceful upgrade stop — it may drain during
# the SIGTERM window, or be stranded claimed-but-not-settled until its lease expires
# and the fresh (upgraded) pool recovers it. Either way: exactly once.
submit_background "$BASE_V1" inflight "at upgrade" >/dev/null
sigterm_expect_graceful 10
pass "old version stopped gracefully (SIGTERM, exit 0) with a run in flight"

# The config-change lands ONLY at restart: the running server never moved off
# PORT_V1 (PART 2 proved you can't reload it live); the NEW process binds PORT_V2.
echo "    restarting the upgraded version over the SAME store, bound to the NEW address :$PORT_V2"
start_server "$PORT_V2"
wait_for_ready "$BASE_V2"
# Observable proof the config change is process-scoped & startup-applied: the old
# address is dead, the new address serves.
curl -sf "$BASE_V1/v1/durable/threads/readyprobe/messages" >/dev/null 2>&1 \
  && fail "old address :$PORT_V1 still serving after the config-changing restart"
pass "new config applied on restart: :$PORT_V1 gone, :$PORT_V2 serving (config is startup-fixed)"

# Durable state is store-scoped: the committed run survives the config change verbatim.
n_committed="$(assistant_count "$BASE_V2" committed)"
[ "$n_committed" -eq 1 ] || fail "thread 'committed' has $n_committed replies after upgrade (expected exactly 1: no loss, no dup)"
pass "thread 'committed' survived the config-changing restart with exactly 1 reply"

# No run is lost across the upgrade: the in-flight run is driven exactly once.
wait_for_assistants "$BASE_V2" inflight 1 "$RECOVERY_TIMEOUT"
# Give any erroneous double-drive a window to also land, then assert it did NOT.
sleep 2
n_inflight="$(assistant_count "$BASE_V2" inflight)"
[ "$n_inflight" -eq 1 ] || fail "thread 'inflight' has $n_inflight replies (expected exactly 1: not lost, not double-driven)"
pass "thread 'inflight' recovered to exactly 1 reply across the upgrade (no loss, no double-drive)"

# The upgraded server serves fresh work too.
submit_background "$BASE_V2" afterupgrade "post upgrade" >/dev/null
wait_for_assistants "$BASE_V2" afterupgrade 1 20
pass "upgraded server keeps draining fresh submissions"

sigterm_expect_graceful 10

echo
echo "FINDING: awaken-server-local has NO live config reload — config is read from env"
echo "once at startup; the only signals are SIGINT/SIGTERM (graceful stop); SIGHUP is"
echo "not a reload signal. A config change requires a full process restart."
echo "CONTRACT: that restart — the unit of a rolling upgrade — is safe: over the durable"
echo "store, a graceful stop + fresh start (even with changed config) drives every"
echo "durable run exactly once (no loss, no duplication) and serves again."
echo
echo "CONFIG RELOAD E2E PASS"
