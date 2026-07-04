//! Agent-domain contract for runtime truth, commits, facts, and neutral events.

pub mod agent;
pub mod commit;
pub mod event;
pub mod fact;
pub mod project;
pub mod secret;
pub mod store;
pub mod stream;

pub use agent::message::Message;
pub use agent::run::{Id as RunId, Record as RunRecord};
pub use agent::state::Key as StateKey;
pub use agent::thread::Id as ThreadId;
pub use commit::coordinator::Coordinator as CommitCoordinator;
pub use commit::staged::{OutboxIntent, ThreadCommit};
pub use event::record::Record as EventRecord;
pub use secret::RedactedString;
pub use store::checkpoint::{CheckpointReader, EventScope};
pub use stream::event::Event as StreamEvent;
pub use stream::sink::Sink as StreamSink;
