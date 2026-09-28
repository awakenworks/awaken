//! The Session realization drivers: stateless executors of the Stage,
//! Activate, Publish/Drain, Acknowledge and terminal-cleanup actions the
//! Session aggregate projects, over contract ports only.
//!
//! Both sides of the topology run them: the Session application drives a
//! Coordinator-local Session, and the Runtime Host's Worker resolver drives a
//! claimed one. Keeping the drivers in neither lets each depend on them
//! without one context reaching into the other's application.

mod session_realization_driver;
mod terminal_cleanup_driver;

pub use session_realization_driver::drive_session_realization;
pub use terminal_cleanup_driver::{
    SessionTerminalCleanupDriveOutcome, drive_session_terminal_cleanup,
};
