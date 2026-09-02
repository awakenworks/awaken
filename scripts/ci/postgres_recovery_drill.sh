#!/usr/bin/env bash
# Physical PostgreSQL point-in-time recovery plus immutable-object restore.
#
# This script deliberately reuses the PostgreSQL and MinIO substrates owned by
# pg_tests.sh. It creates no application repository, recovery state machine, or
# second test manifest: the restored database row and content digest are the
# only oracle, and check-all remains the sole release orchestrator.
set -euo pipefail

valid_lsn() {
  [[ "$1" =~ ^[0-9A-F]+/[0-9A-F]+$ ]]
}

self_test() {
  # Cause/effect graph: C1 a syntactically valid PostgreSQL LSN is supplied;
  # C2 a malformed/empty/shell-shaped value is supplied. Effects: E1 only C1
  # may enter recovery_target_lsn; E2 every C2 partition fails before a config
  # file or container is touched. Decision rules R1=C1=>E1; R2=C2=>E2.
  valid_lsn '0/16B6C50'
  valid_lsn 'A/0'
  ! valid_lsn ''
  ! valid_lsn '0/16b6c50'
  ! valid_lsn "0/1'\nshared_preload_libraries='bad"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

if [ "$#" -ne 3 ]; then
  echo "usage: postgres_recovery_drill.sh <postgres-container> <minio-container> <minio-mc-image>" >&2
  exit 2
fi

PRIMARY_CONTAINER=$1
OBJECT_CONTAINER=$2
MC_IMAGE=$3
RECOVERY_ROOT=${AWAKEN_RECOVERY_ROOT:-}
POSTGRES_IMAGE=${AWAKEN_RECOVERY_POSTGRES_IMAGE:-postgres:16-alpine}
RESTORE_CONTAINER="awaken-ci-pg-restore-$$"
HOST_UID=$(id -u)
HOST_GID=$(id -g)

if [ -z "$RECOVERY_ROOT" ] || [ ! -d "$RECOVERY_ROOT/base" ] || [ ! -d "$RECOVERY_ROOT/wal" ]; then
  echo "AWAKEN_RECOVERY_ROOT must name the exact pg_tests-owned recovery directory" >&2
  exit 2
fi
case "$RECOVERY_ROOT" in
  /tmp/*|/var/tmp/*) ;;
  *) echo "refusing non-temporary recovery root: $RECOVERY_ROOT" >&2; exit 2 ;;
esac

cleanup() {
  docker rm -f "$RESTORE_CONTAINER" >/dev/null 2>&1 || true
  # pg_basebackup and archive_command write as the container's postgres uid.
  # Return ownership of this exact disposable mount to pg_tests.sh so its
  # already-owned cleanup can remove the directory on every failure path.
  docker run --rm --user 0 -v "$RECOVERY_ROOT:/recovery" \
    --entrypoint chown "$POSTGRES_IMAGE" -R "$HOST_UID:$HOST_GID" /recovery \
    >/dev/null 2>&1 || true
}
trap cleanup EXIT

insert_probe() {
  local operation_id=$1
  # psql performs variable substitution for scripts read from stdin. It does
  # not substitute variables in a -c argument, so keeping this as the one
  # insertion path also prevents the three recovery phases from drifting.
  docker exec -i "$PRIMARY_CONTAINER" psql -v ON_ERROR_STOP=1 -U postgres -d awaken \
    -v operation_id="$operation_id" -v object_key="$CONTENT_KEY" \
    -v object_sha256="$CONTENT_SHA256" <<'SQL' >/dev/null
INSERT INTO reliability_recovery_probe
VALUES (:'operation_id', :'object_key', :'object_sha256');
SQL
}

CONTENT_FILE="$RECOVERY_ROOT/content-before-target.txt"
RESTORED_CONTENT="$RECOVERY_ROOT/restored-content.txt"
MANIFEST="$RECOVERY_ROOT/backup-manifest.txt"
BUNDLE="$RECOVERY_ROOT/awaken-recovery.tgz"
DOWNLOADED="$RECOVERY_ROOT/downloaded-awaken-recovery.tgz"
printf 'awaken immutable recovery content\n' >"$CONTENT_FILE"
CONTENT_SHA256=$(sha256sum "$CONTENT_FILE" | awk '{print $1}')
CONTENT_KEY="reliability-recovery/sha256/$CONTENT_SHA256"

# Object cause/effect rule O1: the referenced immutable object is copied into a
# distinct restore bucket before database recovery. A superset is acceptable;
# a missing or digest-mismatched referenced object is not.
docker run --rm --network "container:$OBJECT_CONTAINER" \
  -e CONTENT_KEY="$CONTENT_KEY" -v "$RECOVERY_ROOT:/recovery" \
  --entrypoint /bin/sh "$MC_IMAGE" -ec '
    mc alias set source http://127.0.0.1:9000 minioadmin minioadmin >/dev/null
    mc mb --ignore-existing source/awaken-blobs >/dev/null
    mc mb --ignore-existing source/awaken-blobs-restored >/dev/null
    mc cp /recovery/content-before-target.txt "source/awaken-blobs/$CONTENT_KEY" >/dev/null
    mc mirror --overwrite source/awaken-blobs source/awaken-blobs-restored >/dev/null
  '

docker exec "$PRIMARY_CONTAINER" psql -v ON_ERROR_STOP=1 -U postgres -d awaken -c \
  'DROP TABLE IF EXISTS reliability_recovery_probe; CREATE TABLE reliability_recovery_probe (
     operation_id text PRIMARY KEY,
     object_key text NOT NULL,
     object_sha256 text NOT NULL
   );' >/dev/null
insert_probe base

# The base backup starts from the same live PostgreSQL authority used by every
# preceding backend test. WAL is archived separately so recovery can stop at a
# later committed operation rather than merely reopening a copied data dir.
docker exec -u postgres "$PRIMARY_CONTAINER" \
  pg_basebackup -D /recovery-base -X none --checkpoint=fast

insert_probe at-target
TARGET_LSN=$(docker exec "$PRIMARY_CONTAINER" psql -U postgres -d awaken -tAc \
  'SELECT pg_current_wal_lsn()' | tr -d '[:space:]')
valid_lsn "$TARGET_LSN" || { echo "invalid recovery target LSN: $TARGET_LSN" >&2; exit 1; }
TARGET_SEGMENT=$(docker exec "$PRIMARY_CONTAINER" psql -U postgres -d awaken -tAc \
  'SELECT pg_walfile_name(pg_switch_wal())' | tr -d '[:space:]')
for _ in $(seq 1 120); do
  [ -f "$RECOVERY_ROOT/wal/$TARGET_SEGMENT" ] && break
  sleep 1
done
[ -f "$RECOVERY_ROOT/wal/$TARGET_SEGMENT" ] \
  || { echo "target WAL segment was not archived: $TARGET_SEGMENT" >&2; exit 1; }

insert_probe after-target
AFTER_SEGMENT=$(docker exec "$PRIMARY_CONTAINER" psql -U postgres -d awaken -tAc \
  'SELECT pg_walfile_name(pg_switch_wal())' | tr -d '[:space:]')
for _ in $(seq 1 120); do
  [ -f "$RECOVERY_ROOT/wal/$AFTER_SEGMENT" ] && break
  sleep 1
done
[ -f "$RECOVERY_ROOT/wal/$AFTER_SEGMENT" ] \
  || { echo "post-target WAL segment was not archived: $AFTER_SEGMENT" >&2; exit 1; }

cat >"$MANIFEST" <<EOF
format=awaken-postgres-recovery-v1
recovery_target_lsn=$TARGET_LSN
object_key=$CONTENT_KEY
object_sha256=$CONTENT_SHA256
EOF
# Read the postgres-owned 0700/0600 backup from a root process in the already
# pinned PostgreSQL image. The database files never need weakened permissions.
docker run --rm --user 0 -v "$RECOVERY_ROOT:/recovery" \
  --entrypoint tar "$POSTGRES_IMAGE" \
  -C /recovery -czf /recovery/awaken-recovery.tgz base wal backup-manifest.txt
BUNDLE_SHA256=$(sha256sum "$BUNDLE" | awk '{print $1}')

# The backup crosses the real S3-compatible API before restore. Downloading to
# a distinct path and comparing its digest rejects partial/corrupt transfer.
docker run --rm --network "container:$OBJECT_CONTAINER" \
  -v "$RECOVERY_ROOT:/recovery" --entrypoint /bin/sh "$MC_IMAGE" -ec '
    mc alias set source http://127.0.0.1:9000 minioadmin minioadmin >/dev/null
    mc mb --ignore-existing source/awaken-recovery >/dev/null
    mc cp /recovery/awaken-recovery.tgz source/awaken-recovery/awaken-recovery.tgz >/dev/null
    mc cp source/awaken-recovery/awaken-recovery.tgz \
      /recovery/downloaded-awaken-recovery.tgz >/dev/null
  '
test "$(sha256sum "$DOWNLOADED" | awk '{print $1}')" = "$BUNDLE_SHA256"

mkdir -p "$RECOVERY_ROOT/restore"
tar -C "$RECOVERY_ROOT/restore" -xzf "$DOWNLOADED"
test "$(sed -n 's/^recovery_target_lsn=//p' "$RECOVERY_ROOT/restore/backup-manifest.txt")" = "$TARGET_LSN"
docker run --rm --user 0 \
  -v "$RECOVERY_ROOT/restore/base:/data" \
  -v "$RECOVERY_ROOT/restore/wal:/wal-archive" \
  --entrypoint /bin/sh "$POSTGRES_IMAGE" -ec \
  "touch /data/recovery.signal
   printf '%s\n' \"restore_command = 'cp /wal-archive/%f %p'\" >>/data/postgresql.auto.conf
   printf '%s\n' \"recovery_target_lsn = '$TARGET_LSN'\" >>/data/postgresql.auto.conf
   printf '%s\n' \"recovery_target_action = 'promote'\" >>/data/postgresql.auto.conf
   chown -R postgres:postgres /data /wal-archive"

docker run -d --name "$RESTORE_CONTAINER" \
  -e POSTGRES_PASSWORD=ci -e POSTGRES_DB=awaken \
  -v "$RECOVERY_ROOT/restore/base:/var/lib/postgresql/data" \
  -v "$RECOVERY_ROOT/restore/wal:/wal-archive:ro" \
  "$POSTGRES_IMAGE" >/dev/null
ready=0
for _ in $(seq 1 120); do
  if docker exec "$RESTORE_CONTAINER" pg_isready -U postgres -d awaken >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ "$ready" -ne 1 ]; then
  docker logs "$RESTORE_CONTAINER" >&2
  echo "restored PostgreSQL did not become ready" >&2
  exit 1
fi

# Recovery decision table:
# R1 base + target WAL + valid object => both committed rows and exact object;
# R2 post-target WAL => excluded by recovery_target_lsn; R3 corrupt bundle,
# missing object, or mismatched digest => fail before a success claim.
ROWS=$(docker exec "$RESTORE_CONTAINER" psql -U postgres -d awaken -tAc \
  "SELECT string_agg(operation_id, ',' ORDER BY operation_id) FROM reliability_recovery_probe" \
  | tr -d '[:space:]')
test "$ROWS" = "at-target,base"
RESTORED_KEY=$(docker exec "$RESTORE_CONTAINER" psql -U postgres -d awaken -tAc \
  "SELECT object_key FROM reliability_recovery_probe WHERE operation_id = 'at-target'" \
  | tr -d '[:space:]')
RESTORED_SHA256=$(docker exec "$RESTORE_CONTAINER" psql -U postgres -d awaken -tAc \
  "SELECT object_sha256 FROM reliability_recovery_probe WHERE operation_id = 'at-target'" \
  | tr -d '[:space:]')
test "$RESTORED_KEY" = "$CONTENT_KEY"
test "$RESTORED_SHA256" = "$CONTENT_SHA256"
docker run --rm --network "container:$OBJECT_CONTAINER" \
  -v "$RECOVERY_ROOT:/recovery" --entrypoint /bin/sh "$MC_IMAGE" -ec \
  "mc alias set restored http://127.0.0.1:9000 minioadmin minioadmin >/dev/null
   mc cp restored/awaken-blobs-restored/$CONTENT_KEY /recovery/restored-content.txt >/dev/null"
test "$(sha256sum "$RESTORED_CONTENT" | awk '{print $1}')" = "$RESTORED_SHA256"

echo "✓ PostgreSQL PITR restored the target prefix and exact immutable object"
