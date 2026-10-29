// @formal surface obligation=sandbox.local_process_spawn_policy_and_waiter_lock_ownership_are_preserved
// @formal reason: Original Windows Job assignment, independent control and retained member exit observation.
//! Windows process ownership shared by independent SDK and Worker consumers.
//!
//! A private Job is assigned while the leader is suspended, before workload
//! code runs. Control retains that same Job while a wait is active or cancelled.
//! Before termination, the original Job's current members are retained as wait
//! handles after identity verification. Terminal status requires those handles
//! to report exit and fresh Job accounting to report no active processes, with
//! no new member admitted during that observation. Native query, membership or
//! lock failures remain unconfirmed; a kill request alone is never an exit proof.
#![deny(unsafe_code)]
#![warn(unreachable_pub)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod sys;
#[cfg(windows)]
mod tree;
#[cfg(windows)]
pub use tree::{OwnedProcessTree, ProcessTreeControl};
