//! PostgreSQL storage backend using `sqlx`.
//!
//! Tables are auto-created on first access via `ensure_schema()`.

use sqlx::PgPool;
use tokio::sync::Mutex;

/// PostgreSQL storage backend.
pub struct PostgresStore {
    pub(crate) pool: PgPool,
    pub(crate) threads_table: String,
    pub(crate) runs_table: String,
    pub(crate) messages_table: String,
    pub(crate) configs_table: String,
    config_notify_channel: String,
    schema_ready: Mutex<bool>,
}

impl Clone for PostgresStore {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            threads_table: self.threads_table.clone(),
            runs_table: self.runs_table.clone(),
            messages_table: self.messages_table.clone(),
            configs_table: self.configs_table.clone(),
            config_notify_channel: self.config_notify_channel.clone(),
            schema_ready: Mutex::new(false),
        }
    }
}

impl PostgresStore {
    /// Create a new store with default table names.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            threads_table: "awaken_threads".to_string(),
            runs_table: "awaken_runs".to_string(),
            messages_table: "awaken_messages".to_string(),
            configs_table: "awaken_configs".to_string(),
            config_notify_channel: "awaken_config_changes".to_string(),
            schema_ready: Mutex::new(false),
        }
    }

    /// Create a new store with a custom table prefix.
    pub fn with_prefix(pool: PgPool, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        Self {
            pool,
            threads_table: format!("{prefix}_threads"),
            runs_table: format!("{prefix}_runs"),
            messages_table: format!("{prefix}_messages"),
            configs_table: format!("{prefix}_configs"),
            config_notify_channel: format!("{prefix}_config_changes"),
            schema_ready: Mutex::new(false),
        }
    }

    pub(crate) fn transaction_scope_descriptor(&self) -> String {
        let options = self.pool.connect_options();
        let socket = options
            .get_socket()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        format!(
            "pg::host={}::port={}::socket={}::user={}::database={}::options={}::threads={}::runs={}::messages={}::configs={}",
            options.get_host(),
            options.get_port(),
            socket,
            options.get_username(),
            options.get_database().unwrap_or_default(),
            options.get_options().unwrap_or_default(),
            self.threads_table,
            self.runs_table,
            self.messages_table,
            self.configs_table,
        )
    }
}

mod config;
mod run;
mod schema;
mod thread;

#[cfg(test)]
mod tests;
