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

awaken_exec_without_ambient_api_keys() {
  [ "$#" -gt 0 ] || {
    echo "provider environment sanitizer requires a command" >&2
    return 2
  }
  awaken_unset_ambient_api_keys
  exec "$@"
}

awaken_provider_environment_self_test() {
  # Cause/effect decision table:
  # | inherited name/trigger | child effect |
  # | API_KEY / *_API_KEY, any case | omitted without reading its value |
  # | similarly named file/token or metadata | preserved exactly |
  # | --exec argv and child status | forwarded exactly |
  # | --exec without a command | reject with status 2; start no child |
  # The executable adapter and sourced function share this one name predicate;
  # deterministic JavaScript runners consume only the already-sanitized result.
  local missing_status output status
  if "${BASH_SOURCE[0]}" --exec >/dev/null 2>&1; then
    missing_status=0
  else
    missing_status=$?
  fi
  if output=$(
    env \
      API_KEY=generic \
      DEEPSEEK_API_KEY=provider \
      custom_api_key=lowercase \
      DEEPSEEK_API_KEY_FILE=/projected/key \
      AWAKEN_MCP_BEARER_TOKEN=service \
      "${BASH_SOURCE[0]}" --exec bash -c '
        printf "%s\n" \
          "${API_KEY-unset}" \
          "${DEEPSEEK_API_KEY-unset}" \
          "${custom_api_key-unset}" \
          "${DEEPSEEK_API_KEY_FILE-unset}" \
          "${AWAKEN_MCP_BEARER_TOKEN-unset}" \
          "$1"
        exit 23
      ' _ argv-marker
  ); then
    status=0
  else
    status=$?
  fi
  [ "$missing_status" -eq 2 ] &&
    [ "$status" -eq 23 ] &&
    [ "$output" = $'unset\nunset\nunset\n/projected/key\nservice\nargv-marker' ] || {
    echo "provider environment sanitizer self-test failed" >&2
    return 1
  }
  echo "provider environment sanitizer self-test passed"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-}" in
    --self-test) awaken_provider_environment_self_test ;;
    --exec) shift; awaken_exec_without_ambient_api_keys "$@" ;;
    *) echo "usage: scripts/ci/_provider_environment.sh --self-test | --exec <command> [args...]" >&2; exit 2 ;;
  esac
fi
