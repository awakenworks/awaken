//! AgentOS composition and runtime crate.
#![allow(missing_docs)]

pub use tirea_contract as contracts;

pub(crate) mod loop_engine {
    pub use tirea_agent_loop::engine::*;
}

pub(crate) mod loop_runtime {
    pub use tirea_agent_loop::runtime::*;
}

pub mod composition;
pub mod extensions;
pub mod runtime;
