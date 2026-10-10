#!/usr/bin/env bash
# Validate a commit message first, then commit with it.
#
# git runs the commit-msg hook only after every pre-commit check has finished,
# and on a Rust change those take minutes. A subject one character over the
# limit, or a banned word, therefore cost a full hook run before anyone heard
# about it. This runs the commit-msg validator first -- the same file lefthook's
# `validate-message` command runs, so its rules live in one place only -- and
# refuses within seconds with the validator's own reasons. A message it accepts
# is handed to `git commit -m` unchanged; the commit-msg hook still validates it
# again, so nothing here replaces a check.
#
# usage: scripts/ci/commit.sh "<message>" [git commit arguments...]
#        scripts/ci/commit.sh --self-test
set -euo pipefail

validator="$(cd "$(dirname "$0")" && pwd)/check_commit_message.py"
self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

usage() {
  echo "usage: scripts/ci/commit.sh \"<message>\" [git commit arguments...] | --self-test" >&2
  exit 2
}

# validate <message>: exit status and output of the commit-msg validator.
validate() {
  local message_file status=0
  message_file="$(mktemp)"
  printf '%s\n' "$1" > "$message_file"
  python3 "$validator" "$message_file" || status=$?
  rm -f "$message_file"
  return "$status"
}

self_test() {
  # | Rule | message                         | effect                                        |
  # | C1   | accepted by the validator       | committed with exactly that subject, extra    |
  # |      |                                 | git arguments passed through                  |
  # | C2   | rejected (subject over 100)     | non-zero, validator reason printed, no commit |
  # | C3   | rejected (banned trailer)       | non-zero, validator reason printed, no commit |
  # | C4   | no message at all               | usage, non-zero, no commit                    |
  # | C5   | an unimplemented flag           | usage, non-zero, no commit                    |
  local scratch output good long_subject count
  scratch="$(mktemp -d)"
  trap 'rm -rf "$scratch"' RETURN
  # A disposable repository with no hooks and no user configuration, so the
  # commit this proves is the wrapper's own and not one some hook made.
  export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
  export GIT_AUTHOR_NAME=self-test GIT_AUTHOR_EMAIL=self-test@example.invalid
  export GIT_COMMITTER_NAME=self-test GIT_COMMITTER_EMAIL=self-test@example.invalid
  git -C "$scratch" init --quiet
  commits() { git -C "$scratch" rev-list --count --all 2>/dev/null || echo 0; }

  good="🔧 chore(ci): prove the commit wrapper"
  if ! output="$(cd "$scratch" && "$self" "$good" --allow-empty --quiet 2>&1)"; then
    echo "C1 an accepted message must commit: $output" >&2
    return 1
  fi
  [ "$(git -C "$scratch" log -1 --format=%s)" = "$good" ] || {
    echo "C1 the commit must carry exactly the validated subject" >&2
    return 1
  }
  count="$(commits)"

  long_subject="🔧 chore(ci): $(printf 'x%.0s' $(seq 1 100))"
  if output="$(cd "$scratch" && "$self" "$long_subject" --allow-empty 2>&1)"; then
    echo "C2 an over-long subject must be refused" >&2
    return 1
  fi
  case "$output" in
    *"commit subject exceeds 100 characters"*) ;;
    *) echo "C2 the refusal must carry the validator's reason, got: $output" >&2; return 1 ;;
  esac

  if output="$(cd "$scratch" && "$self" "$good"$'\n\n'"Co-Authored-By: someone <a@b.c>" --allow-empty 2>&1)"; then
    echo "C3 a banned trailer must be refused" >&2
    return 1
  fi
  case "$output" in
    *"Co-Authored-By"*) ;;
    *) echo "C3 the refusal must carry the validator's reason, got: $output" >&2; return 1 ;;
  esac

  for arguments in "" "--awaken-commit-probe"; do
    # shellcheck disable=SC2086 # the empty row must pass no argument at all
    if output="$(cd "$scratch" && "$self" $arguments 2>&1)"; then
      echo "C4/C5 '$arguments' must be refused" >&2
      return 1
    fi
    case "$output" in
      *"usage: scripts/ci/commit.sh"*) ;;
      *) echo "C4/C5 '$arguments' must print usage, got: $output" >&2; return 1 ;;
    esac
  done

  [ "$(commits)" = "$count" ] || {
    echo "C2-C5 a refused message must not create a commit" >&2
    return 1
  }
  echo "commit.sh self-test: the message is validated before git commit runs"
}

case "${1:-}" in
  "") usage ;;
  --self-test)
    [ "$#" -eq 1 ] || usage
    self_test
    exit $?
    ;;
  -*) usage ;;
esac

message="$1"
shift
if ! validate "$message"; then
  echo "commit.sh: nothing was committed; fix the message and rerun." >&2
  exit 1
fi
exec git commit -m "$message" "$@"
