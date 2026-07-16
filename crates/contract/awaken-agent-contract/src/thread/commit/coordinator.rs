use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("commit rejected: {0}")]
    Rejected(String),
}

/// The single durable write boundary (G1/G13). Async because a real store
/// commits over IO; `Send + Sync` so it can be shared as `Arc<dyn Coordinator>`
/// through `RuntimeRunContext`.
#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn commit(
        &self,
        commit: crate::thread::commit::staged::ThreadCommit,
    ) -> Result<crate::thread::commit::staged::CommitRecord, Error>;
}
