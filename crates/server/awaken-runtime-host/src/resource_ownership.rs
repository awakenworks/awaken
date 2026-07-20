//! Workspace ownership projection for content-addressed resources.
//!
//! File bytes are deliberately tenant-neutral and deduplicated; authorization is
//! carried by this separate `(kind, id, workspace)` projection. Thus equal bytes
//! may be owned by multiple workspaces without making the blob id a credential.

use std::collections::BTreeSet;
use std::sync::Mutex;

pub(crate) enum ResourceOwnership {
    Memory(Mutex<BTreeSet<(String, String, String)>>),
    Sqlite(Mutex<rusqlite::Connection>),
}

impl ResourceOwnership {
    pub(crate) fn open(store_dir: Option<&std::path::Path>) -> Self {
        let Some(dir) = store_dir else {
            return Self::Memory(Mutex::new(BTreeSet::new()));
        };
        std::fs::create_dir_all(dir).expect("create resource ownership directory");
        let conn = rusqlite::Connection::open(dir.join("resource-api.db"))
            .expect("open resource ownership database");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS resource_owners (\
                 kind TEXT NOT NULL,\
                 resource_id TEXT NOT NULL,\
                 workspace_id TEXT NOT NULL,\
                 PRIMARY KEY(kind, resource_id, workspace_id)\
             );\
             CREATE INDEX IF NOT EXISTS resource_owners_workspace \
                 ON resource_owners(kind, workspace_id, resource_id);",
        )
        .expect("migrate resource ownership");
        Self::Sqlite(Mutex::new(conn))
    }

    pub(crate) fn grant(&self, kind: &str, id: &str, workspace: &str) {
        match self {
            Self::Memory(rows) => {
                rows.lock().expect("resource owners").insert((
                    kind.to_string(),
                    id.to_string(),
                    workspace.to_string(),
                ));
            }
            Self::Sqlite(conn) => {
                conn.lock()
                    .expect("resource owners")
                    .execute(
                        "INSERT OR IGNORE INTO resource_owners(kind, resource_id, workspace_id) \
                         VALUES (?1, ?2, ?3)",
                        rusqlite::params![kind, id, workspace],
                    )
                    .expect("grant resource ownership");
            }
        }
    }

    pub(crate) fn owns(&self, kind: &str, id: &str, workspace: &str) -> bool {
        match self {
            Self::Memory(rows) => rows.lock().expect("resource owners").contains(&(
                kind.to_string(),
                id.to_string(),
                workspace.to_string(),
            )),
            Self::Sqlite(conn) => conn
                .lock()
                .expect("resource owners")
                .query_row(
                    "SELECT 1 FROM resource_owners \
                     WHERE kind = ?1 AND resource_id = ?2 AND workspace_id = ?3",
                    rusqlite::params![kind, id, workspace],
                    |_| Ok(()),
                )
                .is_ok(),
        }
    }

    pub(crate) fn revoke(&self, kind: &str, id: &str, workspace: &str) -> bool {
        match self {
            Self::Memory(rows) => rows.lock().expect("resource owners").remove(&(
                kind.to_string(),
                id.to_string(),
                workspace.to_string(),
            )),
            Self::Sqlite(conn) => {
                conn.lock()
                    .expect("resource owners")
                    .execute(
                        "DELETE FROM resource_owners \
                         WHERE kind = ?1 AND resource_id = ?2 AND workspace_id = ?3",
                        rusqlite::params![kind, id, workspace],
                    )
                    .expect("revoke resource ownership")
                    > 0
            }
        }
    }

    pub(crate) fn has_any_owner(&self, kind: &str, id: &str) -> bool {
        match self {
            Self::Memory(rows) => rows
                .lock()
                .expect("resource owners")
                .iter()
                .any(|(row_kind, row_id, _)| row_kind == kind && row_id == id),
            Self::Sqlite(conn) => conn
                .lock()
                .expect("resource owners")
                .query_row(
                    "SELECT 1 FROM resource_owners WHERE kind = ?1 AND resource_id = ?2 LIMIT 1",
                    rusqlite::params![kind, id],
                    |_| Ok(()),
                )
                .is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_many_to_many_isolated_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let owners = ResourceOwnership::open(Some(dir.path()));
        owners.grant("file", "hash-1", "ws_a");
        owners.grant("file", "hash-1", "ws_b");
        assert!(owners.owns("file", "hash-1", "ws_a"));
        assert!(owners.owns("file", "hash-1", "ws_b"));
        assert!(!owners.owns("file", "hash-1", "ws_c"));
        assert!(owners.revoke("file", "hash-1", "ws_a"));
        assert!(owners.has_any_owner("file", "hash-1"));
        drop(owners);

        let reopened = ResourceOwnership::open(Some(dir.path()));
        assert!(!reopened.owns("file", "hash-1", "ws_a"));
        assert!(reopened.owns("file", "hash-1", "ws_b"));
    }
}
