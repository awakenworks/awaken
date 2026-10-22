#!/usr/bin/env bash
# One owner for real S3 laws on native and container-backed fixtures.
set -euo pipefail
cd "$(dirname "$0")/../.."
if [ "$#" -ne 1 ]; then echo 'usage: object_store_tests.sh [--admission|--conformance|--seed|--verify]' >&2; exit 2; fi
case "$1" in --admission|--conformance|--seed|--verify) ;; *) echo 'usage: object_store_tests.sh [--admission|--conformance|--seed|--verify]' >&2; exit 2 ;; esac
[ -n "${AWAKEN_TEST_S3_ENDPOINT:-}" ] || { echo 'required S3 fixture endpoint is absent' >&2; exit 2; }
export AWAKEN_TEST_REQUIRE_S3=1
source scripts/ci/_cargo_target.sh
awaken_configure_cargo_target "$PWD"
source scripts/ci/_rust_test_execution.sh
case "$1" in
  --admission)
    run_required_rust_tests -p awaken-file-store --features object-store --lib object::s3_fixture::tests \
      --require-case=object::s3_fixture::tests::required_switch_has_one_canonical_value \
      --require-case=object::s3_fixture::tests::admission_table_preserves_values_and_rejects_missing_invalid_or_empty_fields \
      -- --test-threads=1 ;;
  --conformance)
    run_required_rust_tests -p awaken-file-store --features object-store --lib object::tests::minio_conforms \
      --require-case=object::tests::minio_conforms \
      --require-case=object::tests::minio_conforms_fixture_admission \
      -- --test-threads=1 ;;
  --seed)
    run_required_rust_tests -p awaken-file-store --features object-store --lib object::tests::minio_rotation_seed \
      --require-case=object::tests::minio_rotation_seed -- --exact --test-threads=1 ;;
  --verify)
    run_required_rust_tests -p awaken-file-store --features object-store --lib object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects \
      --require-case=object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects -- --exact --test-threads=1 ;;
esac
