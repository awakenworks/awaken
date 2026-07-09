//! Webhook subscriptions and their persistence port.
//!
//! A subscription is workspace-scoped (ADR-0048: webhooks are a workspace
//! resource) and selects a set of event types. The durable backend is a versioned
//! `awaken-scoped-migration` bundle (requirement: every migration versioned),
//! exactly like the managed session store — the table + ledger live under the
//! `webhook` prefix, isolated within the shared database.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use rusqlite::{Connection, OptionalExtension, params};

/// One webhook subscription: where to deliver, how to sign, and what to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookSubscription {
    pub id: String,
    /// The owning workspace — events for this workspace only reach this endpoint.
    pub workspace_id: String,
    /// The HTTPS endpoint the signed payload is POSTed to.
    pub url: String,
    /// The `whsec_…` signing secret (never delivered; used to sign the payload).
    pub secret: String,
    /// The event types this endpoint receives; empty = all types.
    pub event_types: Vec<String>,
    /// Delivery is suspended (manual, or auto after too many consecutive failures).
    pub disabled: bool,
}

impl WebhookSubscription {
    /// Whether this subscription wants `event_type` (empty `event_types` = all).
    pub fn wants(&self, event_type: &str) -> bool {
        self.event_types.is_empty() || self.event_types.iter().any(|t| t == event_type)
    }
}

/// The port the dispatcher drives to find and update subscriptions.
#[async_trait]
pub trait WebhookRepository: Send + Sync {
    /// Persist (idempotent upsert by id).
    async fn upsert(&self, sub: WebhookSubscription);
    /// Live (non-disabled) subscriptions in `workspace_id` that want `event_type`.
    async fn matching(&self, workspace_id: &str, event_type: &str) -> Vec<WebhookSubscription>;
    /// Fetch one by id.
    async fn get(&self, id: &str) -> Option<WebhookSubscription>;
    /// Set the disabled flag (auto-disable after repeated failures, or manual).
    async fn set_disabled(&self, id: &str, disabled: bool);
}

/// In-memory subscriptions (default / single process / tests).
#[derive(Default)]
pub struct InMemoryWebhookRepository {
    rows: Mutex<HashMap<String, WebhookSubscription>>,
}

#[async_trait]
impl WebhookRepository for InMemoryWebhookRepository {
    async fn upsert(&self, sub: WebhookSubscription) {
        self.rows.lock().unwrap().insert(sub.id.clone(), sub);
    }
    async fn matching(&self, workspace_id: &str, event_type: &str) -> Vec<WebhookSubscription> {
        self.rows
            .lock()
            .unwrap()
            .values()
            .filter(|s| !s.disabled && s.workspace_id == workspace_id && s.wants(event_type))
            .cloned()
            .collect()
    }
    async fn get(&self, id: &str) -> Option<WebhookSubscription> {
        self.rows.lock().unwrap().get(id).cloned()
    }
    async fn set_disabled(&self, id: &str, disabled: bool) {
        if let Some(s) = self.rows.lock().unwrap().get_mut(id) {
            s.disabled = disabled;
        }
    }
}

/// The scoped-migration namespace: table `webhook_subscription`, ledger
/// `webhook_schema_migrations`.
const NS: &str = "webhook";

/// The versioned schema bundle. v1: the subscription row. Every column portable
/// (event_types as a JSON `TEXT`), so a Postgres sibling can share the bundle.
fn subscription_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.webhook_subscription",
        vec![Migration::new(
            1,
            "webhook subscription: one row per subscription id (workspace-scoped)",
            "CREATE TABLE {prefix}_subscription (\
                 id             TEXT PRIMARY KEY, \
                 workspace_id   TEXT NOT NULL, \
                 url            TEXT NOT NULL, \
                 secret         TEXT NOT NULL, \
                 event_types    TEXT NOT NULL, \
                 disabled       INTEGER NOT NULL DEFAULT 0)",
        )?],
    )
}

/// SQLite persistence for [`WebhookSubscription`] over the versioned bundle.
pub struct SqliteWebhookRepository {
    conn: std::sync::Arc<Mutex<Connection>>,
}

impl SqliteWebhookRepository {
    /// Open (or create) the db at `path` and apply the schema migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    /// An in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        let bundle = subscription_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }
}

fn types_json(sub: &WebhookSubscription) -> String {
    serde_json::to_string(&sub.event_types).expect("event_types serialize")
}

fn row_to_sub(
    id: String,
    workspace_id: String,
    url: String,
    secret: String,
    types_json: &str,
    disabled: bool,
) -> WebhookSubscription {
    WebhookSubscription {
        id,
        workspace_id,
        url,
        secret,
        event_types: serde_json::from_str(types_json).unwrap_or_default(),
        disabled,
    }
}

#[async_trait]
impl WebhookRepository for SqliteWebhookRepository {
    async fn upsert(&self, sub: WebhookSubscription) {
        let types = types_json(&sub);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO webhook_subscription
                (id, workspace_id, url, secret, event_types, disabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                workspace_id = excluded.workspace_id,
                url = excluded.url,
                secret = excluded.secret,
                event_types = excluded.event_types,
                disabled = excluded.disabled",
            params![
                sub.id,
                sub.workspace_id,
                sub.url,
                sub.secret,
                types,
                sub.disabled as i64
            ],
        )
        .expect("persist webhook subscription");
    }

    async fn matching(&self, workspace_id: &str, event_type: &str) -> Vec<WebhookSubscription> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, url, secret, event_types, disabled
                 FROM webhook_subscription WHERE workspace_id = ?1 AND disabled = 0",
            )
            .expect("prepare");
        let rows = stmt
            .query_map(params![workspace_id], |row| {
                let types: String = row.get(4)?;
                Ok(row_to_sub(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    &types,
                    row.get::<_, i64>(5)? != 0,
                ))
            })
            .expect("query");
        rows.filter_map(Result::ok)
            .filter(|s| s.wants(event_type))
            .collect()
    }

    async fn get(&self, id: &str) -> Option<WebhookSubscription> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, workspace_id, url, secret, event_types, disabled
             FROM webhook_subscription WHERE id = ?1",
            params![id],
            |row| {
                let types: String = row.get(4)?;
                Ok(row_to_sub(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    &types,
                    row.get::<_, i64>(5)? != 0,
                ))
            },
        )
        .optional()
        .expect("read webhook subscription")
    }

    async fn set_disabled(&self, id: &str, disabled: bool) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE webhook_subscription SET disabled = ?2 WHERE id = ?1",
            params![id, disabled as i64],
        )
        .expect("update webhook subscription");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, ws: &str, types: &[&str]) -> WebhookSubscription {
        WebhookSubscription {
            id: id.to_string(),
            workspace_id: ws.to_string(),
            url: "https://example/hook".to_string(),
            secret: "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".to_string(), // awaken-allow: secret (test sample key)
            event_types: types.iter().map(|s| s.to_string()).collect(),
            disabled: false,
        }
    }

    #[tokio::test]
    async fn matching_is_workspace_scoped_and_type_filtered() {
        let repo = SqliteWebhookRepository::open_in_memory().unwrap();
        repo.upsert(sub("wh_1", "wrkspc_a", &["session.status_idled"]))
            .await;
        repo.upsert(sub("wh_2", "wrkspc_a", &["agent.created"]))
            .await;
        repo.upsert(sub("wh_all", "wrkspc_a", &[])).await; // all types
        repo.upsert(sub("wh_other", "wrkspc_b", &["session.status_idled"]))
            .await;

        let hits = repo.matching("wrkspc_a", "session.status_idled").await;
        let ids: Vec<_> = hits.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"wh_1") && ids.contains(&"wh_all"));
        assert!(!ids.contains(&"wh_2"), "type-filtered out");
        assert!(!ids.contains(&"wh_other"), "other workspace fenced out");
    }

    #[tokio::test]
    async fn disabled_subscriptions_are_excluded_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webhooks.db").to_string_lossy().to_string();
        {
            let repo = SqliteWebhookRepository::open(&path).unwrap();
            repo.upsert(sub("wh_1", "wrkspc_a", &[])).await;
            repo.set_disabled("wh_1", true).await;
            assert!(repo.matching("wrkspc_a", "any").await.is_empty());
        }
        // Reopen: the disabled flag persisted through the versioned bundle.
        let reopened = SqliteWebhookRepository::open(&path).unwrap();
        assert!(reopened.get("wh_1").await.unwrap().disabled);
        assert!(reopened.matching("wrkspc_a", "any").await.is_empty());
    }
}
