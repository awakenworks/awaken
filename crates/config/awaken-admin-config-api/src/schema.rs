//! The admin-config schema (ADR-0043). One portable [`MigrationBundle`] under
//! the `admin` namespace with its own ledger, covering the aggregates the admin
//! plane itself authors (inference profiles, MCP server defs, agent↔MCP
//! bindings) — the catalog/credential domains keep their own bundles. All rows
//! are **secret-free** (an MCP def carries a credential *binding by reference*,
//! never material). Its own bundle prefix is what lets the admin plane be split
//! into its own database/service (blast-radius isolation).

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the admin-config domain.
pub const BUNDLE_ID: &str = "awaken.admin";

const SPECS: [(i64, &str, &str); 5] = [
    (
        1,
        "inference profiles: authored admin-plane aggregates, one JSON row per id",
        "CREATE TABLE {prefix}_inference_profile (\
            id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "mcp servers: authored McpServerDef rows (secret-free; credential is a binding by reference)",
        "CREATE TABLE {prefix}_mcp_server (\
            id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        3,
        "agent mcp bindings: which authored MCP servers an agent uses, one JSON row per agent",
        "CREATE TABLE {prefix}_agent_mcp (\
            agent_id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        4,
        "projects: consumption-side project rows under a workspace (addressing + binding scope)",
        "CREATE TABLE {prefix}_project (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        5,
        "project agent mcp bindings: which MCP servers an agent uses within one project",
        "CREATE TABLE {prefix}_project_agent_mcp (\
            project_id TEXT NOT NULL, \
            agent_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now}, \
            PRIMARY KEY (project_id, agent_id))",
    ),
];

/// Build the admin-schema migration bundle (prefix `admin`).
pub fn admin_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
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
}
