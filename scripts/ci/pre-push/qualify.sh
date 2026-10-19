#!/bin/sh
# A script hook executes even when Lefthook guesses an empty push diff.
set -eu

if [ "$#" -ne 1 ] || [ "$1" != --qualify ]; then
  echo 'usage: qualify.sh --qualify' >&2
  exit 2
fi

# This custom hook has no staged/push-file filter. Its existing gates own scope.
exec lefthook run push-qualification
