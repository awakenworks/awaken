//! The admin-config schema (ADR-0043). One portable [`MigrationBundle`] under
//! the `admin` namespace with its own ledger, covering the aggregates the admin
//! plane itself authors (inference profiles, webhook endpoints, and resource
//! bindings) — the catalog/credential domains keep their own bundles. All rows
//! are **secret-free**. Retired tables remain only as immutable migration history;
//! no current repository adapter reads or writes them. Its own
//! bundle prefix is what lets the admin plane be split into its own
//! database/service (blast-radius isolation).
//!
//! The DDL is NOT encoded in this source: every migration is a `.sql` file under
//! `migrations/`, embedded at build time with `include_str!`. The file name
//! carries the version (`V0005__…` ⇒ version 5) and the first `-- comment` line
//! is its description, so a schema change is a migration *file*, never a Rust
//! string literal.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the admin-config domain.
pub const BUNDLE_ID: &str = "awaken.admin";
pub(crate) const LEGACY_V9_CHECKSUM: &str =
    "43acd8cf624d2d939bff17aadb9c34061bc8f65207007b1c8dd68196a86763c4";
pub(crate) const CURRENT_V9_CHECKSUM: &str =
    "987ffe8ea131956d8b11c59ec8283d880ed97ce89cf02e9e652f61fbc4478131";

pub(crate) fn reconcile_published_v9_receipt(
    checksum: Option<&str>,
    remaining_legacy_table: Option<&str>,
) -> Result<bool, String> {
    if checksum != Some(LEGACY_V9_CHECKSUM) {
        return Ok(false);
    }
    if let Some(table) = remaining_legacy_table {
        return Err(format!(
            "legacy V0009 receipt cannot be reconciled while {table} still exists"
        ));
    }
    Ok(true)
}

/// The embedded migration files, in apply order. Each entry is
/// `(file_name, file_contents)`: the name yields the version, the contents yield
/// the description (first `-- comment` line) and the SQL body. `include_str!`
/// resolves relative to this source file, so the `.sql` files ship in the crate.
const FILES: &[(&str, &str)] = &[
    (
        "V0001__inference_profile.sql",
        include_str!("migrations/V0001__inference_profile.sql"),
    ),
    (
        "V0002__mcp_server.sql",
        include_str!("migrations/V0002__mcp_server.sql"),
    ),
    (
        "V0003__agent_mcp.sql",
        include_str!("migrations/V0003__agent_mcp.sql"),
    ),
    (
        "V0004__agent_resource.sql",
        include_str!("migrations/V0004__agent_resource.sql"),
    ),
    (
        "V0005__webhook.sql",
        include_str!("migrations/V0005__webhook.sql"),
    ),
    (
        "V0006__memory_store.sql",
        include_str!("migrations/V0006__memory_store.sql"),
    ),
    (
        "V0007__webhook_outbox.sql",
        include_str!("migrations/V0007__webhook_outbox.sql"),
    ),
    (
        "V0008__resource_catalog.sql",
        include_str!("migrations/V0008__resource_catalog.sql"),
    ),
    (
        "V0009__retire_legacy_mcp_config.sql",
        include_str!("migrations/V0009__retire_legacy_mcp_config.sql"),
    ),
    (
        "V0010__retire_legacy_webhook_outbox.sql",
        include_str!("migrations/V0010__retire_legacy_webhook_outbox.sql"),
    ),
    (
        "V0011__webhook_mutation_intent.sql",
        include_str!("migrations/V0011__webhook_mutation_intent.sql"),
    ),
];

/// Parse the version from a `Vnnnn__slug.sql` file name (`V0005__…` ⇒ 5). A name
/// that does not carry a positive version yields `0`, which [`Migration::new`]
/// rejects — so a mis-named file fails the bundle build loudly.
fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

/// The migration's description: the first `-- comment` line of the file, so the
/// human-readable summary lives with the DDL rather than in this source.
fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|desc| !desc.is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Build the admin-schema migration bundle (prefix `admin`) from the embedded
/// `.sql` files.
pub fn admin_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_bundle_lints() {
        // Cause/effect decision table: every predecessor migration is applied in
        // order (C1) and the scoped receipt is absent (C2) => V0009..V0011 execute
        // their deterministic DROP statements (E1); receipts present => they are
        // skipped (E2); schema drift/missing predecessors => a bare DROP fails
        // closed (E3), rather than recording a conditional no-op as success.
        let bundle = admin_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn current_v9_checksum_is_a_fixed_compatibility_target() {
        // Cause/effect decision table: the retired V0009 body was republished
        // under the stricter migration policy (C1) and adapters may encounter
        // the exact earlier receipt (C2). Effects: E1 keep the current checksum
        // fixed so the one compatibility translation has a stable target; E2
        // fail before deployment if V0009 is edited again. R1=C1+C2=>E1;
        // R2=edited current body=>E2.
        let bundle = admin_bundle().expect("bundle builds");
        let migration = bundle
            .migrations()
            .iter()
            .find(|migration| migration.version() == 9)
            .expect("V0009 exists");
        assert_eq!(
            migration.checksum_for(awaken_scoped_migration::Dialect::Sqlite),
            CURRENT_V9_CHECKSUM
        );
    }

    #[test]
    fn exact_legacy_v9_receipt_requires_the_retirement_effect() {
        // Cause/effect decision table: C1 receipt equals the one published
        // legacy checksum; C2 no retired table remains; C3 a retired table
        // remains; C4 receipt is absent/current/unknown. Effects: E1 authorize
        // one checksum translation for C1+C2; E2 reject C1+C3; E3 leave C4 to
        // the canonical migration runner. R1=C1+C2=>E1; R2=C1+C3=>E2;
        // R3=C4=>E3. This function is the sole backend-neutral policy owner.
        assert!(reconcile_published_v9_receipt(Some(LEGACY_V9_CHECKSUM), None).unwrap());
        assert!(
            reconcile_published_v9_receipt(Some(LEGACY_V9_CHECKSUM), Some("admin_agent_mcp"))
                .unwrap_err()
                .contains("still exists")
        );
        assert!(!reconcile_published_v9_receipt(Some(CURRENT_V9_CHECKSUM), None).unwrap());
        assert!(!reconcile_published_v9_receipt(Some("unknown"), None).unwrap());
        assert!(!reconcile_published_v9_receipt(None, None).unwrap());
    }

    #[test]
    fn versions_parse_contiguously_from_file_names() {
        // Cause/effect decision table:
        // R1 empty ledger + V1..V11 => build the current Control schema.
        // R2 published prefix + the same V1..V11 => apply only its missing suffix.
        // R3 published prefix + a rewritten V1 => reject unknown/checksum history.
        // Retired DDL is ledger compatibility, not a second repository owner.
        let bundle = admin_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=11).collect::<Vec<_>>());
    }
}
