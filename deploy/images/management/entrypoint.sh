#!/usr/bin/env sh
# Project the existing signed Worker transport contract for the one-container
# topology. The secret is generated once inside the durable data volume, never
# baked into the public image or printed to logs.
set -eu

credential=/var/lib/awaken/worker-transport.json
if test ! -e "$credential"; then
  umask 077
  temporary="${credential}.tmp.$$"
  secret="$(openssl rand -base64 32 | tr -d '\n')"
  printf '%s\n' \
    "{\"worker_id\":\"awaken-worker\",\"key_id\":\"container-local-v1\",\"credential_id\":\"container-local-v1\",\"secret_base64\":\"${secret}\"}" \
    > "$temporary"
  mv "$temporary" "$credential"
fi

exec /usr/local/bin/awaken "$@"
