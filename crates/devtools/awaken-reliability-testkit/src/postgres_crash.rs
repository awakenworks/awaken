//! Select the live crash backend without changing process-global test state.
use std::hash::{Hash, Hasher};
use std::str::FromStr;

fn selected(value: Result<String, std::env::VarError>) -> bool {
    match value {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "postgres" => true,
        _ => panic!("AWAKEN_CRASH_BACKEND must be postgres or unset"),
    }
}

fn schema_name(identity: &str) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    identity.hash(&mut hash);
    format!("awaken_crash_{:016x}", hash.finish())
}

/// Each temporary scenario path identifies one schema shared by parent and
/// killed child. The lane owns an ephemeral database; no existing schema is
/// dropped. Every connection/setup error is fatal once Postgres is selected.
#[expect(
    clippy::disallowed_methods,
    reason = "the reliability fixture reads which live service the test run provides from the environment"
)]
pub async fn postgres_crash_pool(identity: &str) -> Option<sqlx::PgPool> {
    if !selected(std::env::var("AWAKEN_CRASH_BACKEND")) {
        return None;
    }
    let url = crate::postgres_fixture_url().expect("PostgreSQL crash tests require a live fixture");
    let schema = schema_name(identity);
    let admin = awaken_sql_storage::postgres_pool_options()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect crash fixture");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&admin)
        .await
        .expect("create isolated crash schema");
    admin.close().await;
    // Connection startup selects the schema without replacing the canonical
    // checkout callbacks; cancelled transactions must still be retired.
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .expect("parse crash fixture URL")
        .options([("search_path", schema.as_str())]);
    Some(
        awaken_sql_storage::postgres_pool_options()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("open isolated crash schema"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_admission_and_schema_identity_are_exact() {
        // B1 absent => SQLite; B2 exact postgres => required live backend;
        // B3 any typo/invalid encoding => fail, never silently use SQLite.
        assert!(!selected(Err(std::env::VarError::NotPresent)));
        assert!(selected(Ok("postgres".into())));
        for value in [
            Ok("postgre".into()),
            Err(std::env::VarError::NotUnicode("private".into())),
        ] {
            assert!(std::panic::catch_unwind(|| selected(value)).is_err());
        }
        assert_eq!(schema_name("scenario/a"), schema_name("scenario/a"));
        assert_ne!(schema_name("scenario/a"), schema_name("scenario/b"));
        assert!(
            schema_name("unsafe\";DROP SCHEMA public")
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        );
    }
}
