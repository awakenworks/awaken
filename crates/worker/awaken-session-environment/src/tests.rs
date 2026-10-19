//! Session environment tests, split by topic; the shared fakes live in
//! `crate::test_support`.
// Migration coverage: the 64 pre-existing environment test names remain owned
// once, either here (adapter effects) or in Runtime Host (application wiring).
// Cause = tier x create/adopt/rebind/recovery/terminal x current/stale/unknown
// authority; effect = the same filesystem/process/Memory/Repository result or
// fail-closed refusal. Each existing case owns its decision rules; moving the
// adapter must preserve them rather than add a parallel conformance suite.
use super::*;
use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{
    IsolationClass, SandboxProvider, test_support::FenceTamper as _,
};

use awaken_agent_channel::AgentChannel as AgentChannelType;
use awaken_provisioning_contract::SandboxReadiness as _;
use awaken_runtime_contract::llm::ToolCall;
use awaken_session_environment_contract::SessionEnvironmentProvider;
use awaken_tool_contract::tool::ToolExecutor;

use awaken_sandbox_local::FilesystemHandleCodec as _;
use awaken_sandbox_local::NamespaceProvider;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;

mod container_projection;
mod container_transfer;
mod execution_domain;
mod memory_replay;
mod session_hand;
mod shared_environment;

pub(crate) use crate::test_support::*;
