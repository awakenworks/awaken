"""Native fixture admission and independent domain-receipt expectations."""

POSTGRES_PACKAGES = {
    "core": (
        "awaken-reliability-testkit", "awaken-model-catalog-store", "awaken-credential-store",
        "awaken-data-subject-store", "awaken-captured-content-store", "awaken-work-store",
        "awaken-session-store", "awaken-memory-store", "awaken-skill-store", "awaken-work-store",
        "awaken-dispatch-http", "awaken-config-store", "awaken-session-store",
        "awaken-credential-store", "awaken-cli", "awaken-cli", "awaken-coordinator"),
    "storage": (
        "awaken-sql-storage", "awaken-session-store", "awaken-config-store", "awaken-store-postgres",
        "awaken-env-store", "awaken-file-store", "awaken-store-schema", "awaken-store-sqlite"),
    "storage-observations": ("awaken-sql-storage", "awaken-store-postgres"),
    "resources": ('awaken-admin-config-api', 'awaken-resource-store', 'awaken-resource-persistence', 'awaken-session-application', 'awaken-protocol-managed', 'awaken-sandbox-policy-store', 'awaken-executable-agent-catalog', 'awaken-executable-environment-catalog', 'awaken-environment-application', 'awaken-environment-image-build'),
}


def postgres_plan(scope):
    if scope == "core":
        return [("postgres-" + name, name + ".log", ["scripts/ci/pg_tests.sh", "--" + name])
                for name in ("backend-parity", "work-store", "http", "critical")]
    if scope == "storage":
        return [("postgres-storage", "storage.log", ["scripts/ci/pg_tests.sh", "--storage"])]
    if scope == "storage-observations":
        return [("postgres-storage-observations", "storage-observations.log", ["scripts/ci/pg_tests.sh", "--storage-observations"])]
    if scope == "resources":
        return [("postgres-resources", "resources.log", ["scripts/ci/pg_tests.sh", "--resources"])]
    raise ValueError("unsupported native PostgreSQL scope")
