#!/usr/bin/env bash
# Bounded queue over check-all's existing calls; no independent suite manifest.
parallel_checks_init() {
  parallel_jobs="$1"; parallel_directory="$2"; parallel_head=0; parallel_tail=0
  [[ "$parallel_jobs" =~ ^[1-9][0-9]*$ ]] && [ "$parallel_jobs" -le 32 ] || return 2
  parallel_budget="${AWAKEN_STATIC_CHECK_TIMEOUT_SECONDS:-600}"
  [[ "$parallel_budget" =~ ^[1-9][0-9]*$ ]] || return 2
  mkdir -p "$parallel_directory" || return 1
  parallel_owner="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  parallel_pids=(); parallel_labels=(); parallel_commands=()
}

parallel_checks_submit() {
  local label="$1" index="$parallel_tail"; shift
  if [ "${#parallel_pids[@]}" -ge "$parallel_jobs" ]; then
    parallel_checks_wait_one
  fi
  parallel_labels[$index]="$label"
  printf -v "parallel_commands[$index]" '%q ' "$@"
  python3 "$parallel_owner/parallel_check.py" --receipt "$parallel_directory/$index.json" \
    --timeout "$parallel_budget" -- "$@" > "$parallel_directory/$index.log" 2>&1 &
  parallel_pids[$index]=$!
  parallel_tail=$((parallel_tail + 1))
}

parallel_checks_wait_one() {
  [ "${#parallel_pids[@]}" -gt 0 ] || return 0
  local index candidate status=0 duration=0 observation
  # Retire whichever worker finished. Waiting only for the oldest worker left
  # other slots idle behind a long architecture check. Each live worker owns
  # its deadline and cancellation cleanup; absent/corrupt receipts still fail.
  while :; do
    index=""
    for candidate in "${!parallel_pids[@]}"; do
      if [ -e "$parallel_directory/$candidate.json" ] || ! kill -0 "${parallel_pids[$candidate]}" 2>/dev/null; then
        index="$candidate"; break
      fi
    done
    [ -z "$index" ] || break
    sleep 0.05
  done
  wait "${parallel_pids[$index]}" || status=$?
  # A child exit alone cannot prove completion. A missing/corrupt/stale receipt
  # must fail, including an interrupted worker that happens to exit zero.
  if observation=$(python3 - "$parallel_directory/$index.json" "$status" <<'PY'
import json,sys
from pathlib import Path
r=json.loads(Path(sys.argv[1]).read_text())
assert type(r['status']) is int and r['status']==int(sys.argv[2])
assert isinstance(r['duration_seconds'],(int,float)) and r['duration_seconds']>=0
print(r['duration_seconds'])
PY
  ); then duration="$observation"; else status=1; fi
  parallel_checks_result "${parallel_labels[$index]}" "$status" "$duration" \
    "${parallel_commands[$index]}" "$parallel_directory/$index.log"
  parallel_head=$((parallel_head + 1))
  unset 'parallel_pids[index]'
}

parallel_checks_drain() {
  while [ "${#parallel_pids[@]}" -gt 0 ]; do parallel_checks_wait_one; done
}

parallel_checks_cancel() {
  local index
  for index in "${!parallel_pids[@]}"; do
    kill -TERM "${parallel_pids[$index]}" 2>/dev/null || true
  done
  for index in "${!parallel_pids[@]}"; do
    wait "${parallel_pids[$index]}" 2>/dev/null || true
  done
}
