//! Inspection of the actual host/SDK executable, without login or inference.
use super::{npm_launch, process_output};
use crate::{AcpAcquisition, AcpCli};
use awaken_acp_contract::{AcpCommand, AcpExecutableIdentity, AcpExecutableSource};
use std::{process::Stdio, time::Duration};

fn admit_version(cli: &AcpCli, output: &str) -> Result<String, String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .ok_or("ACP_RUNTIME_VERSION_UNAVAILABLE")?;
    if line.len() > 256 || line.chars().any(char::is_control) {
        return Err("ACP_RUNTIME_VERSION_INVALID".into());
    }
    let version = awaken_acp_catalog::parse_version(line).ok_or("ACP_RUNTIME_VERSION_INVALID")?;
    if version < cli.discovery.minimum_version || cli.incompatible_versions.contains(&version) {
        return Err(format!("ACP_RUNTIME_VERSION_UNSUPPORTED: {}", cli.id));
    }
    Ok(line.to_owned())
}

fn version_command(
    cli: &AcpCli,
    launch: &AcpCommand,
) -> Result<(Vec<String>, AcpExecutableSource), String> {
    let (argv, source) = if let Some(args) = cli.runtime_version_args {
        let mut argv = launch.argv.clone();
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        (argv, AcpExecutableSource::AdapterManaged)
    } else if let Some(key) = cli.host_executable_env {
        let program = launch.env.get(key).ok_or("ACP_RUNTIME_BINDING_MISSING")?;
        let mut argv = vec![program.clone()];
        argv.extend(
            cli.discovery
                .version
                .args
                .iter()
                .map(|arg| (*arg).to_string()),
        );
        (argv, AcpExecutableSource::Host)
    } else {
        let AcpAcquisition::Direct { executable, args } = cli.acquisition else {
            return Err("ACP_EXECUTABLE_VERSION_PROBE_UNAVAILABLE".into());
        };
        let prefix_len = launch
            .argv
            .len()
            .checked_sub(args.len())
            .filter(|len| *len > 0)
            .ok_or("ACP_EXECUTABLE_VERSION_COMMAND_INVALID")?;
        if !launch.argv[prefix_len..]
            .iter()
            .map(String::as_str)
            .eq(args.iter().copied())
        {
            return Err("ACP_EXECUTABLE_VERSION_COMMAND_INVALID".into());
        }
        let mut argv = launch.argv[..prefix_len].to_vec();
        let probe = cli.discovery.version;
        if probe.executable != executable {
            // Python console entry points such as hermes/hermes-acp report a
            // shared package version through a sibling in the SAME installation.
            // Never combine that launch with a different installation on PATH.
            if argv.len() != 1 {
                return Err("ACP_EXECUTABLE_VERSION_COMMAND_INVALID".into());
            }
            let selected = std::path::Path::new(&argv[0]);
            let name = if cfg!(windows)
                && selected
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            {
                format!("{}.exe", probe.executable)
            } else {
                probe.executable.to_string()
            };
            argv[0] = selected.with_file_name(name).to_string_lossy().into_owned();
        }
        argv.extend(probe.args.iter().map(|arg| (*arg).to_string()));
        (argv, AcpExecutableSource::Host)
    };
    Ok((argv, source))
}

/// Repeated before every product launch: a CLI upgrade cannot inherit the old
/// process's version evidence. Only the launcher's safe host environment is used.
pub async fn inspect_host_runtime(
    cli: &AcpCli,
    launch: &AcpCommand,
) -> Result<AcpExecutableIdentity, String> {
    let (argv, source) = version_command(cli, launch)?;
    let (program, args) = argv.split_first().ok_or("ACP_RUNTIME_COMMAND_EMPTY")?;
    let mut child = tokio::process::Command::new(program);
    child
        .args(args)
        .env_clear()
        .envs(&launch.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    npm_launch::inherit_host_environment(&mut child);
    let output = process_output::output(&mut child, Duration::from_secs(10))
        .await
        .map_err(|_| "ACP_RUNTIME_VERSION_PROBE_FAILED".to_string())?;
    if output.exit_code != Some(0) {
        return Err("ACP_RUNTIME_VERSION_PROBE_FAILED".into());
    }
    let version = admit_version(cli, &String::from_utf8_lossy(&output.stdout))?;
    Ok(AcpExecutableIdentity {
        cli_id: cli.id.into(),
        cli_version: version,
        source,
        adapter_package: match cli.acquisition {
            AcpAcquisition::PinnedNpmWrapper { package, .. } => Some(package.into()),
            AcpAcquisition::Direct { .. } => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_version_policy_rejects_unknown_old_and_known_bad_releases() {
        let mut cli = *crate::acp_cli("codex").unwrap();
        const BAD_RELEASES: &[crate::AcpVersion] = &[crate::AcpVersion::new(0, 160, 0)];
        cli.incompatible_versions = BAD_RELEASES;
        for output in ["", "unknown", "codex-cli 0.145.9", "codex-cli 0.160.0"] {
            assert!(admit_version(&cli, output).is_err(), "{output}");
        }
        for output in [
            "codex-cli 0.146.0",
            "codex-cli 0.167.0",
            "codex-cli 0.168.0-beta.1",
        ] {
            assert_eq!(admit_version(&cli, output).unwrap(), output);
        }
    }
    #[test]
    fn sdk_version_probe_does_not_report_adapter_version() {
        let cli = crate::acp_cli("claude").unwrap();
        assert_eq!(
            cli.runtime_version_args,
            Some(["--cli", "--version"].as_slice())
        );
        assert!(cli.host_executable_env.is_none());
        assert!(admit_version(cli, "0.69.0").is_err());
        assert!(admit_version(cli, "2.1.221 (Claude Code)").is_ok());
    }
    #[test]
    fn version_probe_uses_the_frozen_executable_and_preserves_js_entry_points() {
        for id in ["opencode", "gemini"] {
            let cli = crate::acp_cli(id).unwrap();
            let AcpAcquisition::Direct { args, .. } = cli.acquisition else {
                panic!()
            };
            // An acquired native binary and an npm JS entry are both prefixes;
            // only the catalog's ACP-mode suffix may be replaced by --version.
            for prefix in [
                vec!["/selected/agent".into()],
                vec!["/selected/node".into(), "/selected/entry.js".into()],
            ] {
                let mut argv = prefix.clone();
                argv.extend(args.iter().map(|arg| (*arg).into()));
                let mut expected = prefix;
                expected.extend(cli.discovery.version.args.iter().map(|arg| (*arg).into()));
                assert_eq!(
                    version_command(cli, &AcpCommand::new(argv)).unwrap(),
                    (expected, AcpExecutableSource::Host)
                );
            }
        }
        let cli = crate::acp_cli("opencode").unwrap();
        assert!(
            version_command(
                cli,
                &AcpCommand::new(vec!["/selected/opencode".into(), "wrong-mode".into()])
            )
            .is_err()
        );
    }

    #[test]
    fn sdk_and_sibling_version_probes_remain_in_the_selected_installation() {
        let claude = crate::acp_cli("claude").unwrap();
        assert_eq!(
            version_command(
                claude,
                &AcpCommand::new(vec!["node".into(), "/selected/claude.js".into()])
            )
            .unwrap(),
            (
                vec![
                    "node".into(),
                    "/selected/claude.js".into(),
                    "--cli".into(),
                    "--version".into()
                ],
                AcpExecutableSource::AdapterManaged
            )
        );
        let hermes = crate::acp_cli("hermes").unwrap();
        let (selected, expected) = if cfg!(windows) {
            ("C:/selected/hermes-acp.exe", "C:/selected/hermes.exe")
        } else {
            ("/selected/hermes-acp", "/selected/hermes")
        };
        let (argv, source) =
            version_command(hermes, &AcpCommand::new(vec![selected.into()])).unwrap();
        assert_eq!(
            std::path::Path::new(&argv[0]),
            std::path::Path::new(expected)
        );
        assert_eq!(&argv[1..], ["version"]);
        assert_eq!(source, AcpExecutableSource::Host);
    }
}
