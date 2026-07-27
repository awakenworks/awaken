//! POSIX-shell resolution for deterministic scenario fixtures.

/// Resolve the shell used by scenario fixtures. Windows npm processes do not
/// necessarily inherit Git for Windows' `bin` directory in `PATH`.
fn scenario_shell() -> String {
    if let Ok(shell) = std::env::var("AWAKEN_E2E_SH")
        && !shell.trim().is_empty()
    {
        return shell;
    }
    #[cfg(windows)]
    for candidate in [
        r"C:\Program Files\Git\bin\sh.exe",
        r"C:\Program Files\Git\usr\bin\sh.exe",
    ] {
        if std::path::Path::new(candidate).is_file() {
            return candidate.to_string();
        }
    }
    "sh".to_string()
}

pub(super) fn scenario_shell_argv(script: &str) -> Vec<String> {
    vec![scenario_shell(), "-c".to_string(), script.to_string()]
}

pub(super) fn scenario_host_acp_cli(
    mut cli: awaken_run_executor_acp::AcpCli,
) -> awaken_run_executor_acp::AcpCli {
    // AcpCli catalog fields are process-lifetime static configuration. The
    // scenario process creates at most one copy per selected router.
    cli.command = Box::leak(scenario_shell().into_boxed_str());
    cli
}
