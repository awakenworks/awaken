//! Host-only composed tests.

#[cfg(target_os = "linux")]
#[path = "session_realizations/execution_authority.rs"]
mod execution_authority;
