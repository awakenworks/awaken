//! The admin-config schema (ADR-0043). One portable [`MigrationBundle`] under
//! the `admin` namespace with its own ledger, covering the aggregates the admin
//! plane itself authors (inference profiles, MCP server defs, agent↔MCP
//! bindings, webhook endpoints) — the catalog/credential domains keep their own
//! bundles. All rows are **secret-free** (an MCP def carries a credential
//! *binding by reference*, a webhook row a `secret_ref`, never material). Its own
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
        let bundle = admin_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_contiguously_from_file_names() {
        let bundle = admin_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=6).collect::<Vec<_>>());
    }
}
