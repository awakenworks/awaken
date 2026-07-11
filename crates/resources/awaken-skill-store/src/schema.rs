//! The skill-store schema. One portable [`MigrationBundle`] under the
//! `skill_store` namespace; a skill is `(workspace_id, id) → content`. The same
//! bundle renders on sqlite and postgres — the schema is written once.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the skill-store domain.
pub const BUNDLE_ID: &str = "awaken.skill_store";

const SPECS: [(i64, &str, &str); 1] = [(
    1,
    "delivered skills: SKILL.md bodies, keyed by (workspace, id)",
    "CREATE TABLE {prefix}_skill (\
        workspace_id TEXT NOT NULL, \
        id TEXT NOT NULL, \
        content TEXT NOT NULL, \
        PRIMARY KEY (workspace_id, id))",
)];

/// Build the skill-store migration bundle (prefix `skill_store`).
pub fn skill_store_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn skill_store_bundle_lints() {
        let bundle = skill_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
