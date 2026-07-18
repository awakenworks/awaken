//! Commit new truth to a thread: the single durable write boundary (G1/G13) and
//! the write aggregate it commits.

pub mod coordinator;
pub mod run;
pub mod run_fact;
pub mod staged;

pub use run::commit_run;
pub use run_fact::RunFact;
pub use staged::RunDisposition;
