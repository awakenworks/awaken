//! Fixtures shared by the Agent runner's boundary, child, and root suites.
//!
//! One home for the publication and message shapes every suite needs, so a
//! suite that moves next to the behaviour it verifies does not fork a second
//! copy of the same fixture.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

pub(super) fn agent(id: &str, instructions: &str) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(id)
        .instructions(instructions)
        .model(ModelBinding::new("default", "stub", "default"))
        .max_steps(4)
        .build()
}

pub(super) fn user(text: &str) -> Message {
    Message {
        id: MessageId("u1".into()),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}
