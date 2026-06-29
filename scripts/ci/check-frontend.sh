#!/usr/bin/env bash
# Run frontend checks when a Node package exists.
set -euo pipefail
cd "$(dirname "$0")/../.."

mode="${1:---full}"

if [ ! -f package.json ]; then
  echo "OK - no package.json; skipping frontend checks."
  exit 0
fi

if ! command -v node >/dev/null 2>&1; then
  echo "ERROR: Node.js is required for frontend checks." >&2
  exit 1
fi

detect_pm() {
  local declared
  declared="$(node -e "const p=require('./package.json').packageManager||''; process.stdout.write(p.split('@')[0]||'')" 2>/dev/null || true)"
  case "$declared" in
    pnpm|npm|yarn|bun) printf '%s\n' "$declared"; return ;;
  esac
  if [ -f pnpm-lock.yaml ]; then printf '%s\n' pnpm; return; fi
  if [ -f package-lock.json ]; then printf '%s\n' npm; return; fi
  if [ -f yarn.lock ]; then printf '%s\n' yarn; return; fi
  if [ -f bun.lockb ]; then printf '%s\n' bun; return; fi
  printf '%s\n' npm
}

pm="$(detect_pm)"
if ! command -v "$pm" >/dev/null 2>&1; then
  echo "ERROR: package manager '$pm' is required for frontend checks." >&2
  exit 1
fi

if [ ! -d node_modules ]; then
  echo "ERROR: frontend dependencies are not installed." >&2
  case "$pm" in
    pnpm) echo "Run: pnpm install --frozen-lockfile" >&2 ;;
    npm) echo "Run: npm ci" >&2 ;;
    yarn) echo "Run: yarn install --immutable" >&2 ;;
    bun) echo "Run: bun install --frozen-lockfile" >&2 ;;
  esac
  exit 1
fi

script_exists() {
  node -e "const s=require('./package.json').scripts||{}; process.exit(s[process.argv[1]]?0:1)" "$1"
}

run_script() {
  local script="$1"
  echo "-> $pm run $script"
  "$pm" run "$script"
}

run_required() {
  local script="$1"
  if ! script_exists "$script"; then
    echo "ERROR: package.json must define a '$script' script." >&2
    exit 1
  fi
  run_script "$script"
}

run_optional() {
  local script="$1"
  if script_exists "$script"; then
    run_script "$script"
  else
    echo "SKIP - package.json has no '$script' script."
  fi
}

run_first_optional() {
  local label="$1"
  shift
  local script
  for script in "$@"; do
    if script_exists "$script"; then
      run_script "$script"
      return
    fi
  done
  echo "SKIP - package.json has no $label script."
}

case "$mode" in
  --quick)
    run_first_optional "format check" format:check fmt:check check:format
    run_required lint
    run_required typecheck
    ;;
  --full)
    run_first_optional "format check" format:check fmt:check check:format
    run_required lint
    run_required typecheck
    run_optional test
    run_required build
    ;;
  *)
    echo "ERROR: unknown mode: $mode (expected --quick or --full)" >&2
    exit 1
    ;;
esac
