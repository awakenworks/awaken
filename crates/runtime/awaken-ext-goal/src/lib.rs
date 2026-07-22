//! Runtime-neutral Outcome domain.
//!
//! Execution and persistence belong to the Runtime Host application layer;
//! backend runtimes consume only ordinary Runs.

pub mod controller;
pub mod grader;
pub mod outcome;
pub mod state;
