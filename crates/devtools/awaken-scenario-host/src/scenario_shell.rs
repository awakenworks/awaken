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

// Cause graph / decision table for scenario-only launch input:
// JSON array + spaces in argv -> preserve exact elements; JSON array without
// spaces -> preserve; legacy plain string -> whitespace split for compatibility;
// malformed JSON-looking input -> legacy split, then launch fails explicitly.
pub(super) fn scenario_argv(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
        raw.split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    })
}

pub(super) fn scenario_host_acp_cli(
    mut cli: awaken_run_executor_acp::AcpCli,
) -> awaken_run_executor_acp::AcpCli {
    // AcpCli catalog fields are process-lifetime static configuration. The
    // scenario process creates at most one copy per selected router.
    let awaken_run_executor_acp::AcpAcquisition::Direct { args, .. } = cli.acquisition else {
        panic!("scenario ACP fixtures must use direct acquisition");
    };
    cli.acquisition = awaken_run_executor_acp::AcpAcquisition::Direct {
        executable: Box::leak(scenario_shell().into_boxed_str()),
        args,
    };
    cli
}

#[cfg(test)]
mod tests {
    use super::scenario_argv;

    #[test]
    fn scenario_argv_preserves_paths_with_spaces() {
        assert_eq!(
            scenario_argv(
                r#"["C:\\Program Files\\nodejs\\node.exe","C:\\fixture dir\\agent.mjs"]"#
            ),
            vec![
                r"C:\Program Files\nodejs\node.exe".to_string(),
                r"C:\fixture dir\agent.mjs".to_string(),
            ]
        );
        assert_eq!(
            scenario_argv("node fixture.mjs"),
            vec!["node".to_string(), "fixture.mjs".to_string()]
        );
    }
}
