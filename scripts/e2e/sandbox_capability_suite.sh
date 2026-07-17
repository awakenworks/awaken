#!/usr/bin/env bash
# Sandbox capability validation suite (G1/G2/G3/G5/G7) — one reproducible runner over
# the REAL substrates each layer needs, self-skipping cleanly when a substrate is
# absent (no silent stubs). Applies a test-design coverage matrix: every capability
# maps to concrete test cases exercising its equivalence classes + boundaries.
#
#   Capability (G)                     | Layer            | Test cases (equivalence / boundary)
#   -----------------------------------+------------------+--------------------------------------------------
#   G2 dispose lifecycle (delete/       | external managed | create->turn->delete->404 ; archive->terminated
#      archive reaps the sandbox)       | protocol (SDK)   | (idempotent re-archive) ; 8x soak (no leak)
#   G2 dispose (host + state)           | rust unit        | end_session_disposes_the_threads_sandbox ;
#                                       |                  | delete/archive_session_disposes*
#   G1 container adapter (docker)       | real docker      | lifecycle ; open_channel dial ; wire exchange
#   G1 mounts / read-only / egress      | real docker      | File/Inline/CacheVolume binds ; :ro rejects ;
#                                       |                  | deny_egress confines (net UP vs DOWN)
#   G5 cgroup enforcement               | real docker      | mem-cap -> OOM 137 ; uncapped -> 0 (boundary)
#   G2 SPOF adopt                       | real docker      | peer re-adopts live handle ; gone -> fail closed
#   G1 k8s pod tier                     | real k3d         | pod lifecycle ; agent-over-wire port-forward ;
#                                       |                  | ConfigMap volume ; blob-source file ; binary data
#   G7 memoryd FUSE write-through       | /dev/fuse host   | read/write/rename/persist-across-remount ;
#                                       |                  | shared-mount coherence ; copy fallback
#   G3 fault != success (fail closed)   | real docker      | OOM/read-only surface non-zero exit, never fake ok
#
# Env: KUBECONFIG (for the k8s layer) — a k3d cluster with busybox:latest + awaken-bb:1
# imported (see k8s_container_e2e.sh). AWAKEN_SKIP_K8S=1 skips the k8s layer.
set -uo pipefail
cd "$(dirname "$0")/../.."

PASS=0; SKIP=0; FAIL=0
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { echo "  PASS: $*"; PASS=$((PASS+1)); }
skip() { echo "  SKIP: $*"; SKIP=$((SKIP+1)); }
run()  { # run <label> <cmd...>
  local label="$1"; shift
  if "$@"; then ok "$label"; else echo "  FAIL: $label"; FAIL=$((FAIL+1)); fi
}

# ── Layer 1: host + managed-state Rust unit tests (always runnable) ───────────
step "G2/G3 host + managed unit tests (dispose, fail-closed)"
# cargo test takes a single substring filter, so run each crate's dispose tests apart.
run "host end_session unit" cargo test -q -p awaken-runtime-host --lib end_session_disposes
run "managed dispose unit" cargo test -q -p awaken-protocol-managed --lib _disposes

# ── Layer 2: external managed-protocol e2e (SDK, no isolation substrate) ──────
step "G2 external managed-protocol dispose lifecycle (official SDK)"
if command -v node >/dev/null && [ -d e2e/node_modules/@anthropic-ai ]; then
  run "managed dispose e2e" bash -c 'cd e2e && node managed_session_dispose_e2e.mjs'
else
  skip "node / @anthropic-ai sdk not installed (run: cd e2e && npm install)"
fi

# ── Layer 3: real Docker daemon (G1 adapter, G5 cgroup, G2 adopt, G3) ─────────
step "G1/G5/G2/G3 container tier against a real Docker daemon"
if docker info >/dev/null 2>&1; then
  docker image inspect busybox:latest >/dev/null 2>&1 || docker pull busybox:latest >/dev/null
  for t in docker_it docker_e2e pairwise_docker; do
    run "docker:$t" cargo test -q -p awaken-sandbox-container --features docker --test "$t"
  done
else
  skip "no reachable Docker daemon"
fi

# ── Layer 4: real Kubernetes (k3d) — G1 pod tier ─────────────────────────────
step "G1 k8s pod tier against a real k3d cluster"
if [ "${AWAKEN_SKIP_K8S:-0}" = 1 ]; then
  skip "AWAKEN_SKIP_K8S=1"
elif [ -n "${KUBECONFIG:-}" ] && kubectl get nodes >/dev/null 2>&1; then
  run "k8s:k8s_it" cargo test -q -p awaken-sandbox-container --features k8s --test k8s_it
  # Ensure the busybox `nc` fixture (awaken-bb:1) exists in the node's containerd, so
  # the process-as-container Pod can start (the node has no registry egress here).
  NODE="$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)"
  if [ -n "$NODE" ] && docker info >/dev/null 2>&1; then
    if ! docker exec "$NODE" crictl images 2>/dev/null | grep -q awaken-bb; then
      docker pull rancher/mirrored-pause:3.6 >/dev/null 2>&1 || true
      docker save rancher/mirrored-pause:3.6 | docker exec -i "$NODE" ctr -n k8s.io images import - >/dev/null 2>&1 || true
      docker rm -f awaken-bb-tmp >/dev/null 2>&1 || true
      docker run --name awaken-bb-tmp busybox:latest true >/dev/null 2>&1 || true
      docker commit awaken-bb-tmp awaken-bb:1 >/dev/null 2>&1 && docker rm -f awaken-bb-tmp >/dev/null 2>&1
      docker save awaken-bb:1 | docker exec -i "$NODE" ctr -n k8s.io images import - >/dev/null 2>&1 || true
    fi
    run "k8s:k8s_e2e" env AWAKEN_K8S_E2E=1 cargo test -q -p awaken-sandbox-container --features k8s --test k8s_e2e
  else
    skip "k8s_e2e: cannot resolve the k3d node to load the awaken-bb:1 fixture"
  fi
else
  skip "no reachable k8s cluster (set KUBECONFIG to a k3d cluster)"
fi

# ── Layer 5: real FUSE (G7 memoryd write-through) ────────────────────────────
step "G7 memoryd FUSE write-through"
if [ -e /dev/fuse ] && { command -v fusermount >/dev/null || command -v fusermount3 >/dev/null; }; then
  run "memoryd fuse kernel_vfs" cargo test -q -p awaken-sandbox-memoryd --test kernel_vfs
  run "sandbox-local memory_mount" cargo test -q -p awaken-sandbox-local --test memory_mount
else
  skip "/dev/fuse or fusermount unavailable (copy fallback path is the alternative)"
fi

# ── Optional: line-coverage of the pure-Rust sandbox-change surface (COVERAGE=1) ─
# The container/k8s/FUSE layers are integration-tested against real substrates
# (measured by their own runs); this reports the lib-testable dispose surface.
if [ "${COVERAGE:-0}" = 1 ] && command -v cargo-llvm-cov >/dev/null; then
  step "line coverage — dispose surface (awaken-protocol-managed + awaken-runtime-host)"
  CARGO_TARGET_DIR="${COV_DIR:-/tmp/awaken-sbx-cov}" \
    cargo llvm-cov --lib -p awaken-protocol-managed -p awaken-runtime-host \
      --summary-only 2>/dev/null | tail -1
  echo "  (uncovered in the change surface: state/sessions.rs child-thread teardown"
  echo "   loop body — no sub-agent child threads in deterministic unit tests; the"
  echo "   {id}:thread:{n} id scheme is validated by the events projection.)"
fi

printf '\n=== SUMMARY: %d passed, %d skipped, %d failed ===\n' "$PASS" "$SKIP" "$FAIL"
[ "$FAIL" -eq 0 ]
