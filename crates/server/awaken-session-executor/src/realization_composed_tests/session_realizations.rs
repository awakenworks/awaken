//! Host-only composed tests.

use awaken_session_realization::session_realizations::*;
#[cfg(target_os = "linux")]
#[path = "session_realizations/execution_authority.rs"]
mod execution_authority;
#[path = "session_realizations/resource_preparation.rs"]
mod resource_preparation;
