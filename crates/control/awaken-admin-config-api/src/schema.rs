//! The admin-config schema (ADR-0043). One portable [`MigrationBundle`] under
//! the `admin` namespace with its own ledger, covering the aggregates the admin
//! plane itself authors (inference profiles, webhook endpoints, and resource
//! bindings) — the catalog/credential domains keep their own
//! bundles. All rows are **secret-free** (a webhook row carries a `secret_ref`,
//! never material). Its own
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
const FILES: &[(&str, &str)] = &[(
    "V0001__control_admin.sql",
    include_str!("migrations/V0001__control_admin.sql"),
)];

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
        // order (C1) and the scoped receipt is absent (C2) => V0009 executes its
        // two deterministic DROP statements (E1); a receipt present => V0009 is
        // skipped (E2); schema drift/missing predecessors => the bare DROP fails
        // closed (E3), rather than recording a conditional no-op as success.
        let bundle = admin_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_contiguously_from_file_names() {
        // Cause/effect: the unreleased Admin history is rebaselined to its one
        // effective owned schema. Exactly V1 is present; retired MCP, outbox,
        // Memory, and Resource-catalog tracks cannot reserve phantom versions.
        let bundle = admin_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1]);
    }
}
