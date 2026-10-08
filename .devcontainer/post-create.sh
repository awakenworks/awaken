#!/usr/bin/env bash
set -euo pipefail
cd /workspaces/awaken
cargo nextest --version >/dev/null 2>&1 || cargo install cargo-nextest --version 0.9.148 --locked
cargo fetch --locked
corepack prepare pnpm@11.6.0 --activate
CI=true pnpm install --frozen-lockfile --store-dir /pnpm/store
npm ci --prefix e2e
echo 'Ready: python3 scripts/ci/rust_test_plan.py --base origin/main --run'
echo 'PostgreSQL crash matrix: AWAKEN_CRASH_BACKEND=postgres cargo test --locked -p awaken-config-store --test process_crash_audit -p awaken-session-store --test process_crash_outbox -p awaken-credential-store --features postgres,sqlite,sealed-aead,test-support --test process_crash_creation --test process_crash_managed_creation -- --test-threads=1 --show-output'
