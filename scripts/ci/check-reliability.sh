#!/usr/bin/env bash
# Cross-layer reliability qualification. Existing domain/conformance tests own
# behavior; this gate composes them once and adds tool-assisted evidence.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/ci/_cargo_target.sh
source scripts/ci/_provider_environment.sh
awaken_configure_cargo_target "$PWD"
awaken_unset_ambient_api_keys

mode="${1:---quick}"
case "$mode" in
  --quick|--full|--require-tools) ;;
  *) echo "usage: scripts/ci/check-reliability.sh [--quick|--full|--require-tools]" >&2; exit 2 ;;
esac

# cargo-mutants restores the source tree, but an interrupted or timestamp-close
# restoration can leave an incremental artifact compiled from the last mutant.
# Every mode starts from source-owned evidence and the mutation mode repeats the
# cleanup on both success and failure.
cleanup_mutation_artifacts() {
  cargo clean -p awaken-mcp-wire >/dev/null 2>&1 || true
  rm -rf -- mutants.out mutants.out.old
}
cleanup_mutation_artifacts
python3 scripts/ci/check_reliability.py
cargo test -p awaken-reliability-testkit --lib
cargo test -p awaken-mcp-wire --lib sse::tests
cargo test -p awaken-run-ingress --features test-support --test dispatch_conformance \
  memory_and_sqlite_refine_the_same_crash_recovery_history

if [ "$mode" = "--quick" ]; then
  echo "reliability quick gate: passed"
  exit 0
fi

cargo test -p awaken-store-sqlite --features test-support --test failure_atomicity
cargo test -p awaken-session-store --test process_crash_outbox
cargo test -p awaken-credential-store --features sqlite,sealed-aead,test-support \
  --test process_crash_creation --test process_crash_managed_creation
cargo test -p awaken-config-store --test process_crash_audit

if [ "$mode" = "--full" ]; then
  echo "reliability full executable gate: passed (tool-assisted gates not requested)"
  exit 0
fi

for tool in cargo-fuzz cargo-mutants; do
  if ! cargo install --list | grep -q "^${tool} "; then
    echo "ERROR: $tool is required by the release reliability gate" >&2
    exit 1
  fi
done
if ! rustup toolchain list | grep -q '^nightly'; then
  echo "ERROR: nightly Rust is required by the release reliability gate" >&2
  exit 1
fi
if ! cargo +nightly miri --version >/dev/null 2>&1; then
  echo "ERROR: the miri component is required by the release reliability gate" >&2
  exit 1
fi

fuzz_seconds="${AWAKEN_FUZZ_SECONDS:-15}"
(cd fuzz && cargo +nightly fuzz run sse_chunks -- -max_total_time="$fuzz_seconds")
(cd fuzz && cargo +nightly fuzz run thread_commit_wire -- -max_total_time="$fuzz_seconds")
cargo +nightly miri test -p awaken-mcp-wire --lib
sanitizer_target="$(rustc +nightly -vV | sed -n 's/^host: //p')"
RUSTFLAGS="-Zsanitizer=address" cargo +nightly test -p awaken-mcp-wire --lib \
  --target "$sanitizer_target" -Zbuild-std
RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -p awaken-reliability-testkit --lib \
  --target "$sanitizer_target" -Zbuild-std
trap cleanup_mutation_artifacts EXIT
cargo mutants --package awaken-mcp-wire \
  --file crates/runtime/awaken-mcp-wire/src/sse.rs --timeout 30 --no-times -- sse::tests
cleanup_mutation_artifacts
trap - EXIT

echo "reliability required-tools gate: passed"
