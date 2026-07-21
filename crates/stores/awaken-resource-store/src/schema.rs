//! Scoped schema for durable resource lifecycle state.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.resource_lifecycle";

const FILES: &[(&str, &str)] = &[(
    "V0001__resource_lifecycle.sql",
    include_str!("migrations/V0001__resource_lifecycle.sql"),
)];

fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.to_string())
}

pub fn resource_lifecycle_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn resource_lifecycle_bundle_lints() {
        let bundle = resource_lifecycle_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
