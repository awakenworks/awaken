#!/bin/sh
case " $* " in *podman-secret*) exit 91;; esac
[ -z "${TOKEN-}" ] || exit 92
alias_name=
previous=
for argument in "$@"; do
  if [ "$previous" = --env ]; then alias_name=$argument; break; fi
  previous=$argument
done
case "$alias_name" in AWAKEN_PODMAN_EXEC_SECRET_*) ;; *) exit 93;; esac
eval "alias_value=\${$alias_name-}"
[ "$alias_value" = podman-secret ] || exit 94
exit 0
