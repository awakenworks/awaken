#!/usr/bin/env bash
# Management image cause-effect graph:
# packaged awaken CLI -> canonical image path + container deployment preset
#   -> one non-root all-in-one process or an explicitly selected split role.
#
# Decision table:
# | Executable path | Command surface        | Result |
# | exact           | version                | version succeeds |
# | exact           | all-in-one + split roles | canonical commands advertised |
# | exact + preset  | default container start | Console responds + setup token logged |
# | absent/wrong    | any                     | container start fails |
set -euo pipefail

IMAGE="${AWAKEN_MANAGEMENT_IMAGE:?set AWAKEN_MANAGEMENT_IMAGE to the image under test}"

version="$(docker run --rm "$IMAGE" version)"
case "$version" in
  "awaken "*) ;;
  *) echo "unexpected management image version output: $version" >&2; exit 1 ;;
esac

help="$(docker run --rm "$IMAGE" --help)"
case "$help" in
  *"all-in-one"*"control"*"coordinator"*"database migrate"*) ;;
  *) echo "management image does not expose the canonical command surface" >&2; exit 1 ;;
esac

docker run --rm --entrypoint /bin/sh "$IMAGE" -ec \
  'test "$(id -u)" = 10001 && test -x /usr/local/bin/awaken && test -r /etc/awaken/config.toml && test ! -e /usr/local/bin/awaken-server'

container="$(docker run --detach --publish 127.0.0.1::8080 "$IMAGE")"
cleanup() {
  docker rm --force --volumes "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT
port="$(docker port "$container" 8080/tcp | sed -n 's/^127\.0\.0\.1://p')"
test -n "$port"
ready=0
for _attempt in $(seq 1 60); do
  if curl --fail --silent "http://127.0.0.1:${port}/" >/dev/null; then
    ready=1
    break
  fi
  if test "$(docker inspect --format '{{.State.Running}}' "$container")" != true; then
    docker logs "$container" >&2
    exit 1
  fi
  sleep 1
done
test "$ready" -eq 1
logs="$(docker logs "$container" 2>&1)"
grep -F "Awaken is ready" <<<"$logs" >/dev/null
grep -F "  Setup     " <<<"$logs" >/dev/null
docker exec "$container" /bin/sh -ec \
  'test "$(stat -c %a /var/lib/awaken/worker-transport.json)" = 600'

echo "management image e2e: canonical CLI, non-root startup, Console, and setup handoff verified"
