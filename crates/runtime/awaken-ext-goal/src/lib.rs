//! Runtime-neutral Outcome domain.
//!
//! The extension owns Outcome execution orchestration and its Thread-state
//! codec. Embedding applications supply only the ordinary Run and Thread ports
//! plus concrete backend/store composition.
#![expect(
    unreachable_pub,
    reason = "pending: narrow the unreachable items to pub(crate)"
)]

pub mod controller;
pub mod grader;
pub mod outcome;
pub mod run_settlement;
pub mod state;
