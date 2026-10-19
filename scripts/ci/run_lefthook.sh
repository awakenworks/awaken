#!/bin/sh
# Generated Git hooks call this entry point through the `lefthook` config key.
set -eu

if [ "$#" -lt 2 ] || [ "$1" != run ]; then
  echo 'usage: run_lefthook.sh run <hook> [hook arguments...]' >&2
  exit 2
fi

# This forces checks to start, not Git to overwrite a remote ref. The Rust/API
# planners still select affected work; pre-commit keeps its staged-file globs.
if [ "$2" = pre-push ]; then
  exec lefthook "$@" --force
fi
exec lefthook "$@"
