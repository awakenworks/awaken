// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Scenario-only fixed-program profile constrains the actual launch before the
// @formal reason: shared ACP executor may enter a restricted Run. This is source classification,
// @formal reason: not qualification of an opaque production adapter.
//! The Outcome matrix's one fixed, tool-free newline program and its profile.

use awaken_run_executor_acp::{AcpLaunch, AcpLaunchIdentity, Codec, OpenError};
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;

/// One tool-free ACP stand-in that can act as either Outcome Worker or Judge.
/// Its prompt classification makes the Worker/Grader backend matrix observable
/// without provider credentials.
const PROGRAM: &str = "read _p; \
    case \"$_p\" in \
      *'Evaluate this Outcome input'*'FINAL answer'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"{\\\"result\\\":\\\"satisfied\\\",\\\"explanation\\\":\\\"ACP judge accepted evidence\\\"}\"}';; \
      *'Evaluate this Outcome input'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"{\\\"result\\\":\\\"needs_revision\\\",\\\"explanation\\\":\\\"ACP judge requests FINAL\\\"}\"}';; \
      *'Revise the deliverable'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"FINAL answer from ACP worker\"}';; \
      *'iteration limit was reached'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"ACP worker acknowledged remaining feedback\"}';; \
      *) printf '%s\\n' '{\"type\":\"message\",\"text\":\"a rough draft from ACP worker\"}';; \
    esac; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'; \
    trap 'exit 0' TERM INT; \
    while read _ignored; do :; done";

/// Pin the ordinary default shell instead of deferring its identity to a child
/// PATH. An explicit alternate interpreter remains usable for Configured Runs,
/// but cannot inherit this program's restricted profile.
pub(super) fn launch() -> AcpLaunch {
    let argv = super::scenario_shell_argv(PROGRAM);
    #[cfg(unix)]
    let argv = {
        let mut argv = argv;
        if argv.first().is_some_and(|program| program == "sh")
            && let Ok(shell) = trusted_shell()
        {
            argv[0] = shell.to_string_lossy().into_owned();
        }
        argv
    };
    let mut launch = AcpLaunch::custom(argv, Vec::new());
    launch.tool_narrowing_check = check;
    launch
}

#[cfg(unix)]
fn trusted_shell() -> Result<std::path::PathBuf, OpenError> {
    std::fs::canonicalize("/bin/sh")
        .map_err(|_| OpenError("fixed Outcome program system shell is unavailable".into()))
}

/// This validates program identity and every supported effective launch input,
/// not an adapter id or a caller-set capability boolean. The stdin value is
/// read as data and matched by `case`; it never becomes an evaluated command.
fn check(
    launch: &AcpLaunch,
    codec: Codec,
    narrowing: &ToolCapabilityNarrowing,
) -> Result<(), OpenError> {
    if narrowing.disabled_reason().is_none() {
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (launch, codec);
        Err(OpenError(
            "fixed Outcome NoTools profile is unverified on this platform".into(),
        ))
    }
    #[cfg(unix)]
    {
        let shell = trusted_shell()?;
        let exact_program = launch.argv.len() == 3
            && std::path::Path::new(&launch.argv[0]) == shell
            && launch.argv[1] == "-c"
            && launch.argv[2] == PROGRAM;
        // The shared local launch owner may add only its PATH/HOME locators.
        // No external commands remain in PROGRAM, and a non-login /bin/sh does
        // not source a HOME file. ENV/BASH_ENV, scripts, secrets and executable
        // overrides never cross this profile's actual env boundary.
        let exact_environment = launch.uses_only_host_lookup_environment();
        let exact_configuration = launch.identity == AcpLaunchIdentity::Managed
            && codec == Codec::Newline
            && launch.session_model.is_none()
            && launch.session_mode.is_none()
            && launch.session_config_options.is_empty()
            && launch.session_working_directory.is_none()
            && launch.tool_aliases.is_empty()
            && launch.expected_capability.is_none()
            && launch.instructions == awaken_run_executor_acp::AcpInstructions::default();
        if exact_program && exact_environment && exact_configuration {
            Ok(())
        } else {
            Err(OpenError(
                "fixed Outcome program does not match its NoTools execution profile".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_sources_do_not_acquire_a_restricted_profile_implicitly() {
        // C1 unrestricted versus DenyAll; C2 custom arbitrary executable versus
        // the owner-checked fixed program. E1 ordinary Configured is unchanged;
        // E2 an opaque launch never inherits support from matching argv/name.
        // R1 custom+Configured=>Ok; R2 same program+opaque+DenyAll=>Err.
        let checked = launch();
        let opaque = AcpLaunch::custom(checked.argv, Vec::new());
        assert!(
            opaque
                .check_tool_narrowing(Codec::Newline, &ToolCapabilityNarrowing::Configured)
                .is_ok()
        );
        assert!(
            opaque
                .check_tool_narrowing(Codec::Newline, &ToolCapabilityNarrowing::DenyAll)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn restricted_program_requires_every_exact_execution_input() {
        // Cause graph: program x interpreter x env x codec x config all identify
        // the same fixed execution. R1 all exact=>NoTools-compatible. R2 each
        // individual changed coordinate=>Err before process entry. R3 adding the
        // canonical host PATH/HOME locators stays valid; an arbitrary shell
        // override or BASH_ENV never becomes an accepted execution profile.
        let original = launch();
        let deny = ToolCapabilityNarrowing::DenyAll;
        assert!(
            original.check_tool_narrowing(Codec::Newline, &deny).is_ok(),
            "R1"
        );
        let mut locators = original.clone();
        locators.env = AcpLaunch::custom(
            Vec::new(),
            vec![
                ("PATH".into(), "/irrelevant".into()),
                ("HOME".into(), "/private".into()),
            ],
        )
        .env;
        assert!(
            locators.check_tool_narrowing(Codec::Newline, &deny).is_ok(),
            "R3"
        );
        assert!(
            original.check_tool_narrowing(Codec::Acp, &deny).is_err(),
            "R2 codec"
        );
        for coordinate in 0..11 {
            let mut changed = original.clone();
            match coordinate {
                0 => changed.argv[2].push_str("; printf tampered"),
                1 => changed.argv.push("extra".into()),
                2 => changed.argv[0] = "/tmp/alternate-interpreter".into(),
                3 => {
                    changed.env =
                        AcpLaunch::custom(Vec::new(), vec![("BASH_ENV".into(), "/tmp/hook".into())])
                            .env
                }
                4 => changed.identity = AcpLaunchIdentity::BackendOwned,
                5 => changed.session_model = Some("other-model".into()),
                6 => changed.session_mode = Some("other-mode".into()),
                7 => changed.session_config_options.push(
                    awaken_protocol_acp::SessionConfigOptionSelection {
                        config_id: "mode".into(),
                        value: "other".into(),
                    },
                ),
                8 => changed.session_working_directory = Some("/other".into()),
                9 => {
                    changed
                        .tool_aliases
                        .insert("Bash".into(), "mcp__other".into());
                }
                10 => {
                    changed.expected_capability =
                        Some(awaken_protocol_acp::AcpCapabilityExpectation {
                            adapter_id: "claude".into(),
                            adapter_version: "any".into(),
                            fingerprint: "unrelated".into(),
                        })
                }
                _ => unreachable!(),
            }
            assert!(
                changed.check_tool_narrowing(Codec::Newline, &deny).is_err(),
                "R2 coordinate {coordinate}"
            );
        }
        let mut instructions = original.clone();
        instructions
            .project_instructions(
                awaken_run_executor_acp::InstructionsDelivery {
                    replace: Some(
                        awaken_run_executor_acp::InstructionsChannel::SessionMetaSystemPrompt,
                    ),
                    append: None,
                },
                "replacement",
            )
            .unwrap();
        assert!(
            instructions
                .check_tool_narrowing(Codec::Newline, &deny)
                .is_err(),
            "R2 instruction delivery"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixed_program_treats_shell_syntax_as_data_and_exits_when_input_closes() {
        // C1 literal shell syntax arrives only through stdin; C2 the checked
        // program emits exactly its original message+turn_end; C3 stdin closes.
        // E1 no marker command executes; E2 closing input reaps this shell
        // naturally within the existing bounded fixture cleanup window. Rules:
        // R1 C1+C2=>E1, R2 R1+C3=>E2. This qualifies this fixed program, not an
        // opaque production CLI; the original API matrix owns Run/Session proof.
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let checked = launch();
        checked
            .check_tool_narrowing(Codec::Newline, &ToolCapabilityNarrowing::DenyAll)
            .unwrap();
        // Positive sensitivity control: the same trusted shell can create the
        // exact marker using only a builtin, so missing PATH cannot make the
        // negative probe pass accidentally.
        let positive = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(&checked.argv[0])
                .args(["-c", ": > injected"])
                .env_clear()
                .current_dir(directory.path())
                .status(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(positive.success());
        assert!(directory.path().join("injected").exists());
        std::fs::remove_file(directory.path().join("injected")).unwrap();
        let mut child = tokio::process::Command::new(&checked.argv[0])
            .args(&checked.argv[1..])
            .env_clear()
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(b"$( : > injected ); `: > injected`; ; : > injected\n")
            .await
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        for expected in ["message", "turn_end"] {
            let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(event["type"], expected);
        }
        drop(input);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            status.success(),
            "R2 natural close reaped the original shell"
        );
        assert!(
            !directory.path().join("injected").exists(),
            "R1 no prompt evaluation"
        );
    }
}
