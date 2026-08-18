#!/usr/bin/env bash
# Keep deterministic checks independent from developer/runner provider secrets.
# Product processes reject these variables too; live suites must project an
# explicit credential through their owned secret-file/FD boundary instead.

awaken_unset_ambient_api_keys() {
  local name normalized
  while IFS= read -r name; do
    normalized="${name^^}"
    if [[ "$normalized" == "API_KEY" || "$normalized" == *_API_KEY ]]; then
      unset "$name"
    fi
  done < <(compgen -e)
}

awaken_provider_environment_self_test() {
  local output
  output=$(
    env \
      API_KEY=generic \
      DEEPSEEK_API_KEY=provider \
      custom_api_key=lowercase \
      DEEPSEEK_API_KEY_FILE=/projected/key \
      AWAKEN_MCP_BEARER_TOKEN=service \
      bash -c '
        source scripts/ci/_provider_environment.sh
        awaken_unset_ambient_api_keys
        printf "%s\n" \
          "${API_KEY-unset}" \
          "${DEEPSEEK_API_KEY-unset}" \
          "${custom_api_key-unset}" \
          "${DEEPSEEK_API_KEY_FILE-unset}" \
          "${AWAKEN_MCP_BEARER_TOKEN-unset}"
      '
  )
  [ "$output" = $'unset\nunset\nunset\n/projected/key\nservice' ] || {
    echo "provider environment sanitizer self-test failed" >&2
    return 1
  }
  echo "provider environment sanitizer self-test passed"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-}" in
    --self-test) awaken_provider_environment_self_test ;;
    *) echo "usage: scripts/ci/_provider_environment.sh --self-test" >&2; exit 2 ;;
  esac
fi
