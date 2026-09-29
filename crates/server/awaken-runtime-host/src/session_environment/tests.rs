//! Host-level Session environment tests: the sandbox-backed adapters under the
//! Host's own Session lifecycle, Resource prompt rendering, Skill delivery and
//! capacity projection. The adapters' own behaviour is tested beside them in
//! `awaken-session-environment`.
use super::*;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_session_environment::test_support::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;

mod container_transfer;
mod shared_environment;
