// @formal surface obligation=credential.managed_creation_publishes_source_and_child_atomically
// @formal surface obligation=credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication
// @formal reason: Credential mutation journal and rollout outbox schema share the Credential
// @formal reason: bounded-context transaction authority.
//! The credential schema (ADR-0043). One portable [`MigrationBundle`] under the
//! `credential` namespace with its own ledger. The `credential_source` rows are
//! **secret-free**; sealed secret material lives in `credential_secret` (written
//! by a sealed-AEAD `SecretStore` backend, P1). Its own bundle prefix is what lets
//! credential be split into its own database/service (blast-radius isolation).

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the credential domain.
pub const BUNDLE_ID: &str = "awaken.credential";

const SPECS: [(i64, &str, &str); 8] = [
    (
        1,
        "credential sources: the secret-free row (kind + refs, never material)",
        "CREATE TABLE {prefix}_source (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "sealed secrets: ciphertext keyed by secret ref (sealed-AEAD backend, P1)",
        "CREATE TABLE {prefix}_secret (\
            secret_ref TEXT PRIMARY KEY, \
            sealed {blob} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        3,
        "credential pools: secret-free failover groupings of sources",
        "CREATE TABLE {prefix}_pool (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        4,
        "credential creation intents: secret-free crash-recovery journal",
        "CREATE TABLE {prefix}_creation_intent (\
            source_id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        5,
        "managed vaults: secret-free management aggregates",
        "CREATE TABLE {prefix}_managed_vault (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        6,
        "managed vault credentials: exact source-backed projections",
        "CREATE TABLE {prefix}_managed_vault_credential (\
            id TEXT PRIMARY KEY, \
            vault_id TEXT NOT NULL, \
            workspace_id TEXT NOT NULL, \
            source_id TEXT NOT NULL UNIQUE, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        7,
        "managed credential mutation: secret-free recovery facts",
        "CREATE TABLE {prefix}_managed_credential_mutation (\
            source_id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        8,
        "managed credential rollout: committed exact-fence service outbox",
        "CREATE TABLE {prefix}_managed_credential_rollout (\
            event_id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
];

/// V9 backfills the Managed Vault/credential `created_at`/`updated_at` facts
/// that rows written before those fields existed never carried.
///
/// Legacy decision: the only real, durable instant such a row has is its own
/// `created_at` column (the insert time, `DEFAULT now`). It becomes both the
/// object's `created_at` and `updated_at`; no mutation history exists to date a
/// later update. Credential snapshots embedded in a pending mutation fact take
/// the instant of the child row they describe (falling back to the mutation
/// row's own insert time for a not-yet-published create), so a snapshot and the
/// durable child it must equal stay byte-identical after the backfill. Only
/// rows lacking the field are touched; the migration is a one-shot data fix.
const TIMESTAMP_BACKFILL_VERSION: i64 = 9;
const TIMESTAMP_BACKFILL_DESCRIPTION: &str =
    "managed vault objects: backfill created_at/updated_at from durable insert time";

const TIMESTAMP_BACKFILL_SQLITE: &str = "\
UPDATE {prefix}_managed_credential_mutation SET data = json_set(data, \
    '$.after_credential.created_at', COALESCE((SELECT strftime('%Y-%m-%dT%H:%M:%SZ', c.created_at) \
        FROM {prefix}_managed_vault_credential c \
        WHERE c.id = json_extract({prefix}_managed_credential_mutation.data, '$.after_credential.id')), \
        strftime('%Y-%m-%dT%H:%M:%SZ', {prefix}_managed_credential_mutation.created_at)), \
    '$.after_credential.updated_at', COALESCE((SELECT strftime('%Y-%m-%dT%H:%M:%SZ', c.created_at) \
        FROM {prefix}_managed_vault_credential c \
        WHERE c.id = json_extract({prefix}_managed_credential_mutation.data, '$.after_credential.id')), \
        strftime('%Y-%m-%dT%H:%M:%SZ', {prefix}_managed_credential_mutation.created_at))) \
WHERE json_type(data, '$.after_credential') = 'object' \
    AND json_type(data, '$.after_credential.created_at') IS NULL;
UPDATE {prefix}_managed_credential_mutation SET data = json_set(data, \
    '$.before_credential.created_at', COALESCE((SELECT strftime('%Y-%m-%dT%H:%M:%SZ', c.created_at) \
        FROM {prefix}_managed_vault_credential c \
        WHERE c.id = json_extract({prefix}_managed_credential_mutation.data, '$.before_credential.id')), \
        strftime('%Y-%m-%dT%H:%M:%SZ', {prefix}_managed_credential_mutation.created_at)), \
    '$.before_credential.updated_at', COALESCE((SELECT strftime('%Y-%m-%dT%H:%M:%SZ', c.created_at) \
        FROM {prefix}_managed_vault_credential c \
        WHERE c.id = json_extract({prefix}_managed_credential_mutation.data, '$.before_credential.id')), \
        strftime('%Y-%m-%dT%H:%M:%SZ', {prefix}_managed_credential_mutation.created_at))) \
WHERE json_type(data, '$.before_credential') = 'object' \
    AND json_type(data, '$.before_credential.created_at') IS NULL;
UPDATE {prefix}_managed_vault_credential SET data = json_set(data, \
    '$.created_at', strftime('%Y-%m-%dT%H:%M:%SZ', created_at), \
    '$.updated_at', strftime('%Y-%m-%dT%H:%M:%SZ', created_at)) \
WHERE json_type(data, '$.created_at') IS NULL;
UPDATE {prefix}_managed_vault SET data = json_set(data, \
    '$.created_at', strftime('%Y-%m-%dT%H:%M:%SZ', created_at), \
    '$.updated_at', strftime('%Y-%m-%dT%H:%M:%SZ', created_at)) \
WHERE json_type(data, '$.created_at') IS NULL;";

const TIMESTAMP_BACKFILL_POSTGRES: &str = "\
UPDATE {prefix}_managed_credential_mutation AS m SET data = jsonb_set(jsonb_set(m.data, \
    '{after_credential,created_at}', to_jsonb(to_char(COALESCE((SELECT c.created_at \
        FROM {prefix}_managed_vault_credential c WHERE c.id = m.data->'after_credential'->>'id'), \
        m.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))), \
    '{after_credential,updated_at}', to_jsonb(to_char(COALESCE((SELECT c.created_at \
        FROM {prefix}_managed_vault_credential c WHERE c.id = m.data->'after_credential'->>'id'), \
        m.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))) \
WHERE jsonb_typeof(m.data->'after_credential') = 'object' \
    AND m.data->'after_credential'->'created_at' IS NULL;
UPDATE {prefix}_managed_credential_mutation AS m SET data = jsonb_set(jsonb_set(m.data, \
    '{before_credential,created_at}', to_jsonb(to_char(COALESCE((SELECT c.created_at \
        FROM {prefix}_managed_vault_credential c WHERE c.id = m.data->'before_credential'->>'id'), \
        m.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))), \
    '{before_credential,updated_at}', to_jsonb(to_char(COALESCE((SELECT c.created_at \
        FROM {prefix}_managed_vault_credential c WHERE c.id = m.data->'before_credential'->>'id'), \
        m.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))) \
WHERE jsonb_typeof(m.data->'before_credential') = 'object' \
    AND m.data->'before_credential'->'created_at' IS NULL;
UPDATE {prefix}_managed_vault_credential SET data = jsonb_set(jsonb_set(data, \
    '{created_at}', to_jsonb(to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))), \
    '{updated_at}', to_jsonb(to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))) \
WHERE data->'created_at' IS NULL;
UPDATE {prefix}_managed_vault SET data = jsonb_set(jsonb_set(data, \
    '{created_at}', to_jsonb(to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))), \
    '{updated_at}', to_jsonb(to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))) \
WHERE data->'created_at' IS NULL;";

/// Build the credential-schema migration bundle (prefix `credential`).
pub fn credential_bundle() -> Result<MigrationBundle, MigrationError> {
    credential_bundle_through(SPECS.len() + 1)
}

/// The bundle as published through `count` migrations. Tests use a prefix to
/// materialize a pre-upgrade database and then apply the rest for real.
fn credential_bundle_through(count: usize) -> Result<MigrationBundle, MigrationError> {
    let mut migrations = SPECS[..count.min(SPECS.len())]
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    if count > SPECS.len() {
        migrations.push(Migration::per_dialect(
            TIMESTAMP_BACKFILL_VERSION,
            TIMESTAMP_BACKFILL_DESCRIPTION,
            TIMESTAMP_BACKFILL_POSTGRES,
            TIMESTAMP_BACKFILL_SQLITE,
        )?);
    }
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(test)]
mod tests;
