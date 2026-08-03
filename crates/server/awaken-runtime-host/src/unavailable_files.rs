//! Fail-closed File ports for a database-less execution Worker.
//!
//! Workers receive immutable bytes through `FileContentSource`; they do not own
//! Files API metadata or blob authority. These ports make accidental management
//! access observable as an error instead of silently creating process-local truth.

use awaken_resource_contract::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, FileStore, FileStoreError,
};

const UNAVAILABLE: &str = "File authority is unavailable on an execution Worker";

pub(crate) struct UnavailableWorkerFiles;

#[async_trait::async_trait]
impl FileStore for UnavailableWorkerFiles {
    async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
        Err(FileStoreError(UNAVAILABLE.into()))
    }

    async fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        Err(FileStoreError(UNAVAILABLE.into()))
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        Err(FileStoreError(UNAVAILABLE.into()))
    }

    async fn delete(&self, _id: &str) -> Result<bool, FileStoreError> {
        Err(FileStoreError(UNAVAILABLE.into()))
    }
}

#[async_trait::async_trait]
impl FileCatalog for UnavailableWorkerFiles {
    async fn create_file(
        &self,
        _record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        Err(FileCatalogError::Storage(UNAVAILABLE.into()))
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(UNAVAILABLE.into()))
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(UNAVAILABLE.into()))
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(UNAVAILABLE.into()))
    }

    async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
        Err(FileCatalogError::Storage(UNAVAILABLE.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_worker_file_authority_operation_fails_closed() {
        // Cause/effect graph: C1 operation targets blob/catalog authority; C2 Host
        // role is database-less Worker. Effects: E1 no state is created/read/listed/
        // deleted; E2 caller receives an explicit storage error. Constraint: claim-
        // fenced immutable reads use FileContentSource and never these ports.
        //
        // | Rule | port    | operation class       | effect |
        // | T1   | blob    | create/read/list/delete| E1,E2 |
        // | T2   | catalog | create/read/list/delete/account | E1,E2 |
        let files = UnavailableWorkerFiles;
        assert!(files.put(b"bytes").await.is_err(), "T1 put");
        assert!(files.get("id").await.is_err(), "T1 get");
        assert!(files.list().await.is_err(), "T1 list");
        assert!(files.delete("id").await.is_err(), "T1 delete");
        assert!(
            files
                .create_file(FileRecord {
                    id: "file".into(),
                    workspace_id: "workspace".into(),
                    blob_id: "blob".into(),
                    filename: "file.txt".into(),
                    mime_type: "text/plain".into(),
                    size_bytes: 1,
                    created_at: "2026-01-01T00:00:00Z".into(),
                    downloadable: true,
                    scope_id: None,
                    logical_path: None,
                    harvest_key: None,
                    deleted: false,
                })
                .await
                .is_err(),
            "T2 create"
        );
        assert!(
            files.get_file("workspace", "file", false).await.is_err(),
            "T2 get"
        );
        assert!(
            files.list_files("workspace", None).await.is_err(),
            "T2 list"
        );
        assert!(
            files.mark_file_deleted("workspace", "file").await.is_err(),
            "T2 delete"
        );
        assert!(
            files.active_size_bytes("workspace").await.is_err(),
            "T2 account"
        );
    }
}
