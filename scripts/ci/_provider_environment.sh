#!/usr/bin/env bash
# Keep deterministic checks independent from developer/runner provider secrets
# and developer-only substrate opt-outs. Product processes reject ambient
# provider variables too; live suites must project an explicit credential
# through their owned secret-file/FD boundary instead.

awaken_unset_ambient_api_keys() {
  local name
  while IFS= read -r name; do
    # Match the same case-insensitive names with Bash 3.2 builtins. Spawning
    # printf/tr for every inherited name makes parallel native Windows probes
    # spend their admission budget starting unrelated utility processes.
    case "$name" in
      [aA][pP][iI]_[kK][eE][yY]|*_[aA][pP][iI]_[kK][eE][yY]) unset "$name" ;;
    esac
  done < <(compgen -e)
}

# Tool homes this repository's gates depend on but a non-login shell does not
# see. A gate that cannot find `kubectl` or `z3` does not report a missing tool;
# it reports whatever the tool's absence happens to break, which has read as a
# code failure more than once, and once as six failed SMT assertions.
#
# These are *appended*, never prepended. The job is to make an otherwise
# unfindable tool findable, not to decide which tool a name selects: a caller
# that put a directory on PATH on purpose — a test fixture owning its own
# instrumented `cargo`, say — must keep winning. Only existing directories are
# added and nothing is installed, so a genuinely absent tool still fails its own
# gate by name.
awaken_append_repo_tools() {
  local candidate
  for candidate in \
    "${XDG_BIN_HOME:-$HOME/.local/bin}" \
    "$HOME/.local/share/awaken-formal/tools/tlapm/lib/tlapm/backends/bin"; do
    if [ -d "$candidate" ] && [[ ":$PATH:" != *":$candidate:"* ]]; then
      PATH="$PATH:$candidate"
    fi
  done
  export PATH
}

awaken_exec_deterministic_environment() {
  [ "$#" -gt 0 ] || {
    echo "provider environment sanitizer requires a command" >&2
    return 2
  }
  awaken_unset_ambient_api_keys
  unset AWAKEN_E2E_ALLOW_MISSING_CONTAINER_ENGINES
  exec "$@"
}

awaken_provider_environment_self_test() {
  # Cause/effect decision table:
  # | inherited name/trigger | child effect |
  # | API_KEY / *_API_KEY, any case | omitted without reading its value |
  # | AWAKEN_E2E_ALLOW_MISSING_CONTAINER_ENGINES | omitted at canonical --exec |
  # | similarly named file/token or metadata | preserved exactly |
  # | --exec argv and child status | forwarded exactly |
  # | --exec without a command | reject with status 2; start no child |
  # | an existing tool home absent from PATH | appended once, never in front |
  # | the same tool home already on PATH | left where it is, not duplicated |
  # | a tool home that does not exist | never added |
  # Direct stage-script execution does not cross this adapter, so a developer
  # can still opt out explicitly there; the release aggregate cannot inherit it.
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
      AWAKEN_E2E_ALLOW_MISSING_CONTAINER_ENGINES=1 \
      DEEPSEEK_API_KEY_FILE=/projected/key \
      AWAKEN_MCP_BEARER_TOKEN=service \
      "${BASH_SOURCE[0]}" --exec bash -c '
        printf "%s\n" \
          "${API_KEY-unset}" \
          "${DEEPSEEK_API_KEY-unset}" \
          "${custom_api_key-unset}" \
          "${AWAKEN_E2E_ALLOW_MISSING_CONTAINER_ENGINES-unset}" \
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
    [ "$output" = $'unset\nunset\nunset\nunset\n/projected/key\nservice\nargv-marker' ] || {
    echo "provider environment sanitizer self-test failed" >&2
    return 1
  }
  awaken_provider_name_predicate_self_test || return 1
  awaken_repo_tools_self_test || return 1
  echo "provider environment sanitizer self-test passed"
}

awaken_provider_name_predicate_self_test() (
  # Cross all 64 letter-case combinations with exact/prefixed names. Near
  # misses remain byte-for-byte present; no real credential value is read.
  local mask index name
  local upper=(A P I K E Y) lower=(a p i k e y) removed=()
  local retained=(APIKEY CUSTOMAPI_KEY API_KEY_FILE prefix_API_KEY_SUFFIX)
  for mask in {0..63}; do
    name=""
    for index in {0..5}; do
      if (( mask & (1 << index) )); then
        name="${name}${lower[$index]}"
      else
        name="${name}${upper[$index]}"
      fi
      [ "$index" -ne 2 ] || name="${name}_"
    done
    export "$name=fixture" "prefix_$name=fixture"
    removed+=("$name" "prefix_$name")
  done
  for name in "${retained[@]}"; do export "$name=fixture"; done
  awaken_unset_ambient_api_keys
  for name in "${removed[@]}"; do
    if declare -p "$name" >/dev/null 2>&1; then
      echo "provider name predicate retained a credential name" >&2
      return 1
    fi
  done
  for name in "${retained[@]}"; do
    [ "${!name}" = fixture ] || { echo "provider name predicate changed a near miss" >&2; return 1; }
  done
)

awaken_repo_tools_self_test() {
  local root present absent before after
  root="$(mktemp -d)"
  present="$root/present"
  absent="$root/absent"
  mkdir -p "$present"
  (
    HOME="$root"
    XDG_BIN_HOME="$present"
    PATH="$absent:/usr/bin:/bin"
    before="$PATH"
    awaken_append_repo_tools
    after="$PATH"
    [[ ":$after:" == *":$present:"* ]] || { echo "an existing tool home was not added" >&2; exit 1; }
    [[ ":$after:" != *":$root/.local/share/awaken-formal"* ]] ||
      { echo "a nonexistent tool home was added" >&2; exit 1; }
    # The caller's own selection must still win, so the addition goes last.
    [[ "$after" == "$before:$present" ]] || { echo "a tool home displaced the caller's PATH" >&2; exit 1; }
    awaken_append_repo_tools
    [[ "$PATH" == "$after" ]] || { echo "a repeated append duplicated a tool home" >&2; exit 1; }
  ) || { rm -rf -- "$root"; return 1; }
  rm -rf -- "$root"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-}" in
    --self-test) awaken_provider_environment_self_test ;;
    --exec) shift; awaken_exec_deterministic_environment "$@" ;;
    *) echo "usage: scripts/ci/_provider_environment.sh --self-test | --exec <command> [args...]" >&2; exit 2 ;;
  esac
fi
