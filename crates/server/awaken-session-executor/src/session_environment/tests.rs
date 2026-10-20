//! Host-level Session environment tests: the sandbox-backed adapters under the
//! Host's own Session lifecycle, Resource prompt rendering, Skill delivery and
//! capacity projection. The adapters' own behaviour is tested beside them in
//! `awaken-session-environment`.
use super::*;
use awaken_provisioning_contract as pc;
use awaken_session_environment::test_support::*;
use awaken_tool_contract::tool::ToolCall;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;

#[path = "tests/container_transfer.rs"]
mod container_transfer;
#[path = "tests/shared_environment.rs"]
mod shared_environment;
