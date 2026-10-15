#!/usr/bin/env bash
# Generate the management-plane TS contract from the Rust DTOs (ADR-0043),
# mirroring oversight-next's pipeline:
#   Rust #[derive(JsonSchema)]  ->  export_schemas (JSON Schema SSOT)  ->  TS types.
#
# We generate TS only for OUR surfaces (admin config API). The Managed Agents wire
# remains owned by the official @anthropic-ai/sdk; its Rust compatibility DTOs are
# exported as reviewable JSON Schema and never used to generate a rival SDK.
#
#   scripts/contract/generate-contracts.sh          # regenerate committed artifacts
#   scripts/contract/generate-contracts.sh --check   # fail if committed artifacts are stale
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
out_dir="$repo_root/contracts"
schema_json="$out_dir/model-schemas.generated.json"
openapi_json="$out_dir/openapi.generated.json"
ts_out="$out_dir/model-config.d.ts"

# `--check` was matched with a bare test and every other argument ignored, so a
# misspelling silently REGENERATED the committed contracts instead of verifying
# them -- the one outcome this gate exists to distinguish.
case "${1:-}" in
  ""|--check) ;;
  *) echo "usage: scripts/contract/generate-contracts.sh [--check]" >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/contract/generate-contracts.sh [--check]" >&2
  exit 2
fi
model_selection_ts="$out_dir/model-selection.generated.d.ts"
managed_agents_json="$out_dir/anthropic-managed/canonical-wire.schemas.generated.json"
check=0
[ "${1:-}" = "--check" ] && check=1

mkdir -p "$out_dir"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

python3 "$repo_root/scripts/contract/test_schema_to_typescript.py"

# 1. JSON Schema SSOT from the Rust DTOs, plus the OpenAPI 3.1 document (route
#    registry over the same schemas; drives frontend openapi-typescript codegen).
cargo run --locked -q -p awaken-admin-config-api --features schema --example export_admin_schemas \
    > "$tmp/schemas.json"
cargo run --locked -q -p awaken-admin-config-api --features schema --example export_openapi \
    > "$tmp/openapi.generated.json"
cargo run --locked -q -p awaken-protocol-managed --features schema --example export_managed_schemas \
    > "$tmp/managed-agents.schemas.json"

# Control-owned MCP read/config DTOs share the same management contract output.
cargo run --locked -q -p awaken-control --features schema --example export_mcp_schemas > "$tmp/mcp.schemas.json"
python3 - "$tmp/schemas.json" "$tmp/openapi.generated.json" "$tmp/mcp.schemas.json" <<'PYMCP'
import json,pathlib,sys
schemas_path,openapi_path,mcp_path=map(pathlib.Path,sys.argv[1:])
extra=json.loads(mcp_path.read_text(encoding='utf-8'))
for path,is_openapi in [(schemas_path,False),(openapi_path,True)]:
    root=json.loads(path.read_text(encoding='utf-8'))
    target=root['components']['schemas'] if is_openapi else root['schemas']
    for key,value in extra["schemas"].items():
        if key in target and target[key]!=value: raise ValueError('conflicting schema '+key)
        target[key]=value
    if is_openapi:
        for key,value in extra["paths"].items():
            if key in root["paths"] and root["paths"][key]!=value: raise ValueError('conflicting path '+key)
            root["paths"][key]=value
    if is_openapi:
        # Each exported schema owns its nested definitions. Fragment-only refs
        # must be rebased when the document embeds them under components.
        def rebase(value, name):
            if isinstance(value,list): return [rebase(v,name) for v in value]
            if not isinstance(value,dict): return value
            result={k:rebase(v,name) for k,v in value.items()}
            ref=result.get("$ref", "")
            if ref.startswith("#/$defs/"):
                result["$ref"]="#/components/schemas/"+name+ref[1:]
            return result
        root["components"]["schemas"]={k:rebase(v,k) for k,v in target.items()}
        # Reject a published reference that cannot resolve within this document.
        def check_refs(value):
            if isinstance(value,list):
                for v in value: check_refs(v)
            elif isinstance(value,dict):
                ref=value.get("$ref", "")
                if ref.startswith("#/"):
                    node=root
                    for part in ref[2:].split("/"): node=node[part.replace("~1","/").replace("~0","~")]
                for v in value.values(): check_refs(v)
        check_refs(root)
    path.write_text(json.dumps(root,indent=2,ensure_ascii=False,sort_keys=True)+'\n', encoding='utf-8', newline='\n')
PYMCP

# 2. TS types via the lockfile-pinned generator. The helper merges every root
#    and shared definition into one conflict-checked `$defs` graph so json2ts
#    retains JSON-Schema discriminated unions instead of flattening their fields.
generated_ts=0
if command -v pnpm >/dev/null 2>&1; then
    # The WindowsApps `python3.exe` shim is discoverable but not executable from
    # Git Bash. Prefer the real `python.exe` on Windows; elsewhere retain the
    # conventional python3-first lookup.
    if [[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* ]]; then
        python_bin=python
    elif command -v python3 >/dev/null 2>&1; then
        python_bin=python3
    else
        python_bin=python
    fi
    # ModelSelection is also shipped as a deliberately narrow standalone file
    # for the existing console import. Extract only that root; the complete
    # model-config declaration is generated from the merged graph below.
    "$python_bin" - "$tmp/schemas.json" "$tmp/model-selection.schema.json" <<'PY'
import json, pathlib, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
schema = data["schemas"]["ModelSelection"]
schema.setdefault("title", "ModelSelection")
pathlib.Path(sys.argv[2]).write_text(json.dumps(schema), encoding="utf-8", newline="\n")
PY
    "$python_bin" "$repo_root/scripts/contract/schema_to_typescript.py" build \
        "$tmp/schemas.json" "$tmp/model-config.codegen.json"
    if pnpm exec json2ts \
        --output "$tmp/model-config.d.ts" \
        --unreachableDefinitions < "$tmp/model-config.codegen.json" >/dev/null; then
        "$python_bin" "$repo_root/scripts/contract/schema_to_typescript.py" \
            finalize-stdin "$tmp/model-config.d.ts"
        printf '// GENERATED by scripts/contract/generate-contracts.sh — do not edit.\n' \
            > "$tmp/final.d.ts"
        cat "$tmp/model-config.d.ts" >> "$tmp/final.d.ts"
        "$python_bin" "$repo_root/scripts/contract/schema_to_typescript.py" assert \
            "$tmp/model-config.codegen.json" "$tmp/final.d.ts"
        generated_ts=1
    else
        echo "warn: json2ts unavailable/failed; emitting JSON Schema only" >&2
    fi
    if [ "$generated_ts" = 1 ]; then
        "$python_bin" "$repo_root/scripts/contract/schema_to_typescript.py" normalize \
            "$tmp/model-selection.schema.json" "$tmp/model-selection.codegen.json"
        pnpm exec json2ts \
            "$tmp/model-selection.codegen.json" \
            --output "$tmp/model-selection.generated.d.ts" >/dev/null
        printf '// GENERATED by scripts/contract/generate-contracts.sh — do not edit.\n' \
            > "$tmp/model-selection.final.d.ts"
        cat "$tmp/model-selection.generated.d.ts" >> "$tmp/model-selection.final.d.ts"
        mv "$tmp/model-selection.final.d.ts" "$tmp/model-selection.generated.d.ts"
    fi
else
    echo "error: pnpm is required to generate the TypeScript contracts" >&2
fi

if [ "$generated_ts" != 1 ]; then
    echo "error: contract generation is incomplete; no generated artifacts were written" >&2
    exit 1
fi

if [ "$check" = 1 ]; then
    diff -u "$schema_json" "$tmp/schemas.json" >/dev/null \
        || { echo "✗ $schema_json is stale — run scripts/contract/generate-contracts.sh"; exit 1; }
    diff -u "$openapi_json" "$tmp/openapi.generated.json" >/dev/null \
        || { echo "✗ $openapi_json is stale — run scripts/contract/generate-contracts.sh"; exit 1; }
    diff -u "$ts_out" "$tmp/final.d.ts" >/dev/null \
        || { echo "✗ $ts_out is stale — run scripts/contract/generate-contracts.sh"; exit 1; }
    diff -u "$model_selection_ts" "$tmp/model-selection.generated.d.ts" >/dev/null \
        || { echo "✗ $model_selection_ts is stale — run scripts/contract/generate-contracts.sh"; exit 1; }
    diff -u "$managed_agents_json" "$tmp/managed-agents.schemas.json" >/dev/null \
        || { echo "✗ $managed_agents_json is stale — run scripts/contract/generate-contracts.sh"; exit 1; }
    pnpm --filter @awaken/managed-sdk-oracle check
    echo "OK - contracts up to date."
else
    cp "$tmp/schemas.json" "$schema_json"
    cp "$tmp/openapi.generated.json" "$openapi_json"
    cp "$tmp/final.d.ts" "$ts_out"
    cp "$tmp/model-selection.generated.d.ts" "$model_selection_ts"
    cp "$tmp/managed-agents.schemas.json" "$managed_agents_json"
    pnpm --filter @awaken/managed-sdk-oracle generate
    echo "wrote $schema_json + $openapi_json + $ts_out + $model_selection_ts + $managed_agents_json"
fi
