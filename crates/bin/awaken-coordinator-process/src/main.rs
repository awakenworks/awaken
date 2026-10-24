//! `awaken-coordinator` — the Coordinator-only service executable.
//!
//! It runs the shared process startup in the Coordinator role. A Coordinator
//! never owns a claim pool, so this package supplies no co-located Worker and
//! its dependency closure links none.
#![warn(unreachable_pub)]

use std::process::ExitCode;

fn main() -> ExitCode {
    awaken_process_startup::block_on_service(|| {
        awaken_process_startup::run_service_binary(awaken_process_startup::ServiceRole::Coordinator)
    })
}
