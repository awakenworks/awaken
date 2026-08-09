#!/usr/bin/env bash
# Canonical host-build/staging boundary for every sandbox image fixture. Cargo may
# write outside ./target, and a caller may enforce a restrictive umask; resolve the
# executable from Cargo's JSON stream and install an explicitly executable copy.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../.." && pwd)
staged=${1:?usage: stage-binary.sh OUTPUT [FEATURES]}
features=${2:-}

if command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1; then
  build_python=python3
elif command -v python >/dev/null 2>&1 && python -c 'import sys' >/dev/null 2>&1; then
  build_python=python
else
  echo "a working Python 3 interpreter is required" >&2
  exit 1
fi

cargo_args=(build --release -p awaken-sandbox --message-format=json)
[[ -z "$features" ]] || cargo_args+=(--features "$features")
cd "$repo"
bin=$(cargo "${cargo_args[@]}" 2>/dev/null \
  | "$build_python" -c "import sys,json
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get('reason') == 'compiler-artifact' and m.get('target', {}).get('name') == 'awaken-sandbox' and m.get('executable'):
        print(m['executable'])" \
  | tail -1)
[[ -n "$bin" ]] || { echo "could not resolve the awaken-sandbox binary" >&2; exit 1; }

install -m 0755 "$bin" "$staged"
