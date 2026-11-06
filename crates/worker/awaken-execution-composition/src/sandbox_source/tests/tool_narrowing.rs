//! Restriction preflight consumes frozen environment facts, never provider names.
use super::*;
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;

fn checked_boundary(
    _launch: &AcpLaunch,
    _codec: awaken_run_executor_acp::Codec,
    narrowing: &ToolCapabilityNarrowing,
) -> Result<(), OpenError> {
    if narrowing.disabled_reason().is_some() {
        // This sentinel cannot grant execution. Its distinct error proves the
        // environment guard delegated only the exact supported input to the
        // source's checker. Scenario's physical program/matrix owns success.
        Err(OpenError("fixed program checker consulted".into()))
    } else {
        Ok(())
    }
}

#[test]
fn restricted_source_requires_a_known_empty_namespace_environment() {
    // C/E decision table: C1 Namespace rather than Workdir/container; C2 frozen
    // baseline env known empty, unknown, or authored nonempty; C3 no attached
    // channels. Configured MCP metadata is omitted by the original Fixed/newline
    // projection (row5), so it cannot create an extra admission authority.
    // Only C1+known-empty+C3 reaches the concrete program
    // checker; every other row refuses before materialize/spawn. Configured
    // remains unchanged. The sentinel never attests a runnable NoTools profile.
    for row in 0..7 {
        let mut sandbox = CapturingAgentSandbox {
            container: false,
            ..CapturingAgentSandbox::default()
        };
        match row {
            3 => sandbox.container = true,
            4 => sandbox.host_identity = true,
            _ => {}
        }
        let sandbox = Arc::new(sandbox);
        let mut launch = AcpLaunch::custom(vec!["fixed".into()], Vec::new());
        launch.tool_narrowing_check = checked_boundary;
        let environment_variables = match row {
            1 => None,
            2 => Some(
                AcpLaunch::custom(Vec::new(), vec![("BASH_ENV".into(), "/authored".into())]).env,
            ),
            _ => Some(Vec::new()),
        };
        let mut source = BoundLocalChannelSource::from_environment(
            sandbox.clone(),
            LaunchSource::Fixed(Box::new(launch)),
            awaken_runtime_contract::resolved::Backend::from_ref("acp:claude"),
            Vec::new(),
            environment_variables,
        );
        if row == 5 {
            source
                .mcp_servers
                .push(awaken_acp_contract::SessionMcpServer {
                    name: "unproven".into(),
                    command: None,
                    args: Vec::new(),
                    url: Some("http://127.0.0.1:1".into()),
                    auth: None,
                });
        }
        if row == 6 {
            let exporter = Arc::new(super::attached::Exporter {
                calls: std::sync::atomic::AtomicUsize::new(0),
                drops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail: false,
            });
            source.attached_tools = Some(Arc::new(
                awaken_session_environment_contract::tool_export::AttachedToolSet::new(
                    exporter,
                    Vec::new(),
                    Vec::new(),
                )
                .unwrap(),
            ));
        }
        let error = source
            .check_tool_narrowing(&ToolCapabilityNarrowing::DenyAll)
            .unwrap_err();
        assert_eq!(
            error.0 == "fixed program checker consulted",
            matches!(row, 0 | 5),
            "row {row}"
        );
        assert!(
            source
                .check_tool_narrowing(&ToolCapabilityNarrowing::Configured)
                .is_ok()
        );
        assert!(sandbox.command.lock().unwrap().is_none());
        assert!(sandbox.materialized.lock().unwrap().is_empty());
    }
}
