#!/usr/bin/env bash
# Management image cause-effect graph:
# host-built awaken CLI -> canonical image path -> Helm command selection
#   -> database migrate OR management process.
#
# Decision table:
# | Executable path | Command surface        | Result |
# | exact           | version                | version succeeds |
# | exact           | database + management  | both commands advertised |
# | absent/wrong    | any                    | container start fails |
set -euo pipefail

IMAGE="${AWAKEN_MANAGEMENT_IMAGE:?set AWAKEN_MANAGEMENT_IMAGE to the image under test}"

version="$(docker run --rm "$IMAGE" version)"
case "$version" in
  "awaken "*) ;;
  *) echo "unexpected management image version output: $version" >&2; exit 1 ;;
esac

help="$(docker run --rm "$IMAGE" --help)"
case "$help" in
  *"management"*"database migrate"*) ;;
  *) echo "management image does not expose both required commands" >&2; exit 1 ;;
esac

docker run --rm --entrypoint /bin/sh "$IMAGE" -ec \
  'test -x /usr/local/bin/awaken && test ! -e /usr/local/bin/awaken-server'

echo "management image e2e: canonical CLI path and command surface verified"
