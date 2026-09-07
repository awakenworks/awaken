#!/usr/bin/env bash
# One allocation/cleanup grammar for pg_tests and its physical recovery drill.
postgres_recovery_parent() {
  (cd -- "${TMPDIR:-/tmp}" && pwd -P)
}

postgres_recovery_prefix() {
  printf '%s/awaken-pg-recovery.' "${1%/}"
}

valid_recovery_root() {
  local prefix suffix
  prefix=$(postgres_recovery_prefix "$RECOVERY_PARENT")
  case "$1" in
    "$prefix"*) suffix=${1#"$prefix"} ;;
    *) return 1 ;;
  esac
  # '?' in a case pattern also admits slashes. Only mktemp's exact leaf
  # alphabet is eligible; an existing symlink never grants cleanup authority.
  [[ "$suffix" =~ ^[a-zA-Z0-9]{6}$ ]] && [ ! -L "$1" ]
}
