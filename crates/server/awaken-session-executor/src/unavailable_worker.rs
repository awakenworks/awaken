//! Fail-closed authority ports for a database-less execution Worker.
//!
//! Workers receive immutable bytes through `FileContentSource`; they do not own
//! Files API metadata/blob authority or the durable Memory extraction outbox.
//! These ports make accidental management access observable as an error instead
//! of silently creating process-local truth.

use awaken_resource_contract::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, FileStore, FileStoreError,
};

const FILE_UNAVAILABLE: &str = "File authority is unavailable on an execution Worker";
const EXTRACTION_UNAVAILABLE: &str =
    "Memory extraction authority is unavailable on an execution Worker";

pub(crate) struct UnavailableWorkerFiles;

pub(crate) struct UnavailableWorkerExtractions;

#[async_trait::async_trait]
impl FileStore for UnavailableWorkerFiles {
    async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
        Err(FileStoreError(FILE_UNAVAILABLE.into()))
    }

    async fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        Err(FileStoreError(FILE_UNAVAILABLE.into()))
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        Err(FileStoreError(FILE_UNAVAILABLE.into()))
    }

    async fn delete(&self, _id: &str) -> Result<bool, FileStoreError> {
        Err(FileStoreError(FILE_UNAVAILABLE.into()))
    }
}

#[async_trait::async_trait]
impl FileCatalog for UnavailableWorkerFiles {
    async fn create_file(
        &self,
        _record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        Err(FileCatalogError::Storage(FILE_UNAVAILABLE.into()))
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(FILE_UNAVAILABLE.into()))
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(FILE_UNAVAILABLE.into()))
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Err(FileCatalogError::Storage(FILE_UNAVAILABLE.into()))
    }

    async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
        Err(FileCatalogError::Storage(FILE_UNAVAILABLE.into()))
    }
}

#[async_trait::async_trait]
impl awaken_ext_memory::MemoryExtractionRepository for UnavailableWorkerExtractions {
    async fn put_extraction_if_absent(
        &self,
        _intent: awaken_ext_memory::MemoryExtractionIntent,
    ) -> Result<
        awaken_ext_memory::PutMemoryExtractionOutcome,
        awaken_ext_memory::MemoryExtractionError,
    > {
        Err(awaken_ext_memory::MemoryExtractionError::Storage(
            EXTRACTION_UNAVAILABLE.into(),
        ))
    }

    async fn get_extraction(
        &self,
        _intent_id: &str,
    ) -> Result<
        Option<awaken_ext_memory::MemoryExtractionIntent>,
        awaken_ext_memory::MemoryExtractionError,
    > {
        Err(awaken_ext_memory::MemoryExtractionError::Storage(
            EXTRACTION_UNAVAILABLE.into(),
        ))
    }

    async fn extraction_cursor(
        &self,
        _session_id: &str,
    ) -> Result<usize, awaken_ext_memory::MemoryExtractionError> {
        Err(awaken_ext_memory::MemoryExtractionError::Storage(
            EXTRACTION_UNAVAILABLE.into(),
        ))
    }

    async fn recoverable_extractions(
        &self,
        _limit: usize,
    ) -> Result<
        Vec<awaken_ext_memory::MemoryExtractionIntent>,
        awaken_ext_memory::MemoryExtractionError,
    > {
        Err(awaken_ext_memory::MemoryExtractionError::Storage(
            EXTRACTION_UNAVAILABLE.into(),
        ))
    }

    async fn compare_and_swap_extraction(
        &self,
        _expected_revision: u64,
        _intent: awaken_ext_memory::MemoryExtractionIntent,
    ) -> Result<(), awaken_ext_memory::MemoryExtractionError> {
        Err(awaken_ext_memory::MemoryExtractionError::Storage(
            EXTRACTION_UNAVAILABLE.into(),
        ))
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

    #[tokio::test]
    async fn every_worker_extraction_authority_operation_fails_closed() {
        use awaken_ext_memory::MemoryExtractionRepository as _;

        // Cause/effect graph: C1 operation targets the durable extraction outbox;
        // C2 execution runs on a database-less Worker. Effects: E1 no intent,
        // cursor, lease, or receipt is stored/read; E2 an explicit storage error
        // is returned. Constraint: remote terminal commits are observed by the
        // Coordinator's authoritative Host after its commit succeeds.
        //
        // | Rule | operation class         | C1 | C2 | effect |
        // | T1   | create/read/cursor/list | Y  | Y  | E1,E2  |
        // | T2   | compare-and-swap        | Y  | Y  | E1,E2  |
        let repository = UnavailableWorkerExtractions;
        let intent = awaken_ext_memory::MemoryExtractionIntent::new_range(
            "intent",
            "session:run",
            "workspace",
            "session",
            "run",
            "memory",
            1,
            0,
            0,
            Vec::new(),
            awaken_ext_memory::MemoryExtractorSnapshot::host_executor(
                "memory-agent",
                "provider",
                "model",
                "default",
            ),
        )
        .expect("valid extraction intent");

        assert!(
            repository
                .put_extraction_if_absent(intent.clone())
                .await
                .is_err(),
            "T1 create"
        );
        assert!(
            repository.get_extraction("intent").await.is_err(),
            "T1 read"
        );
        assert!(
            repository.extraction_cursor("session").await.is_err(),
            "T1 cursor"
        );
        assert!(
            repository.recoverable_extractions(10).await.is_err(),
            "T1 list"
        );
        assert!(
            repository
                .compare_and_swap_extraction(0, intent)
                .await
                .is_err(),
            "T2 CAS"
        );
    }
}
