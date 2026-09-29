use super::*;

const TEXT: &str = "  冻结规则：保持身份；引号\"、\\、{env:DO_NOT_EXPAND}\n第二行。\n";

fn empty_launch() -> AcpLaunch {
    AcpLaunch::custom(vec!["fixture".into()], Vec::new())
}

fn config(launch: &AcpLaunch, name: &str) -> serde_json::Value {
    let variable = launch.env.iter().find(|entry| entry.name == name).unwrap();
    let pc::EnvValue::Inline { value } = &variable.value else {
        panic!("inline config")
    };
    serde_json::from_str(value).unwrap()
}

#[test]
fn every_catalog_route_delivers_once_and_declares_its_identity_exception() {
    // Causes: C1 every catalog row; C2 managed/backend-owned identity; C3 file
    // route has a Session-owned home. Effects: E1 system append; E2 explicit
    // first-prompt fallback; E3 exact file bytes and no repeated binding.
    // Rules I1 all except Gemini+backend-owned => E1; I2 that combination=>E2;
    // I3 file+C3=>E1+E3. No fixture is credited as vendor-model acceptance.
    for cli in crate::known_acp_clis() {
        for identity in [AcpLaunchIdentity::Managed, AcpLaunchIdentity::BackendOwned] {
            let mut launch = empty_launch();
            launch.identity = identity;
            launch.project_instructions(cli.instructions, TEXT).unwrap();
            let file = launch.bind_instructions_file("/session/private").unwrap();
            if cli.id == "gemini" && identity == AcpLaunchIdentity::BackendOwned {
                assert_eq!(
                    launch.instructions.placement(),
                    InstructionsPlacement::FirstPrompt {
                        reason: "backend_owned_home"
                    },
                    "I2"
                );
            } else {
                assert_eq!(
                    launch.instructions.placement(),
                    InstructionsPlacement::SystemAppend,
                    "I1 {}",
                    cli.id
                );
            }
            if let Some(file) = file {
                assert_eq!(file.contents, TEXT, "I3 exact frozen bytes");
                assert!(
                    launch
                        .bind_instructions_file("/session/private")
                        .unwrap()
                        .is_none(),
                    "I3 one binding"
                );
                if cli.id == "opencode" {
                    assert_eq!(
                        config(&launch, "OPENCODE_CONFIG_CONTENT")["instructions"],
                        serde_json::json!(["/session/private/.awaken/agent-instructions.md"])
                    );
                }
            }
            if cli.id == "claude" {
                assert_eq!(
                    launch.instructions.session_meta(),
                    Some(&serde_json::json!({"append":TEXT}))
                );
            }
            if cli.id == "codex" {
                assert_eq!(
                    config(&launch, "CODEX_CONFIG")["developer_instructions"],
                    TEXT
                );
            }
            if cli.id == "hermes" {
                assert!(
                    launch
                        .env
                        .iter()
                        .any(|entry| entry.name == "HERMES_ENVIRONMENT_HINT"
                            && entry.value == pc::EnvValue::Inline { value: TEXT.into() })
                );
            }
        }
        assert!(
            !cli.supports_tool_aliases || cli.instructions.replace.is_some(),
            "built-in replacement must have a real route"
        );
    }
}

#[test]
fn replacement_and_empty_instructions_have_explicit_outcomes() {
    // Causes: C1 aliases replace built-ins; C2 nonempty/blank instructions;
    // C3 replace route exists. R1 !C1+blank=>Absent; R2 C1+blank+C3=>empty
    // replacement; R3 C1+text+C3=>SystemReplace; R4 C1+!C3=>error.
    for cli in crate::known_acp_clis() {
        let mut launch = empty_launch();
        launch
            .project_instructions(cli.instructions, " \n")
            .unwrap();
        assert_eq!(
            launch.instructions.placement(),
            InstructionsPlacement::Absent,
            "R1"
        );
        launch
            .tool_aliases
            .insert("Bash".into(), "mcp__awaken_session__bash".into());
        assert_eq!(
            launch.project_instructions(cli.instructions, "").is_ok(),
            cli.instructions.replace.is_some(),
            "R2/R4"
        );
        if cli.id == "claude" {
            assert_eq!(
                launch.instructions.session_meta(),
                Some(&serde_json::json!("\n")),
                "R2 truthy transport, semantically empty instructions"
            );
        }
        let result = launch.project_instructions(cli.instructions, TEXT);
        assert_eq!(
            result.is_ok(),
            cli.instructions.replace.is_some(),
            "R3/R4 {}",
            cli.id
        );
        if result.is_ok() {
            assert_eq!(
                launch.instructions.placement(),
                InstructionsPlacement::SystemReplace
            );
            if cli.id == "claude" {
                assert_eq!(
                    launch.instructions.session_meta(),
                    Some(&serde_json::json!(TEXT))
                );
            }
            if cli.id == "codex" {
                assert_eq!(config(&launch, "CODEX_CONFIG")["instructions"], TEXT);
            }
        }
    }
}

#[test]
fn config_merge_preserves_provider_and_rejects_invalid_or_secret_config() {
    // Causes: C1 object/nonobject/malformed/secret config; C2 preexisting
    // provider and model; C3 append key. Effects: E1 preserve unrelated config;
    // E2 never rewrite invalid/secret input. M1 object+C2=>E1; M2 others=>E2.
    let delivery = crate::acp_cli("codex").unwrap().instructions;
    let mut launch = empty_launch();
    set_inline(&mut launch.env, "CODEX_CONFIG", r#"{"model":"pinned","model_providers":{"awaken":{"base_url":"https://provider.invalid"}}}"#.into());
    let before = config(&launch, "CODEX_CONFIG");
    launch.project_instructions(delivery, TEXT).unwrap();
    let after = config(&launch, "CODEX_CONFIG");
    assert_eq!(after["model"], before["model"], "M1");
    assert_eq!(after["model_providers"], before["model_providers"], "M1");
    assert_eq!(after["developer_instructions"], TEXT);
    for value in [
        pc::EnvValue::Inline { value: "[]".into() },
        pc::EnvValue::Inline { value: "{".into() },
        pc::EnvValue::Secret {
            reference: "secret://must-not-read".into(),
        },
    ] {
        let mut launch = empty_launch();
        launch.env.push(pc::EnvVar {
            name: "CODEX_CONFIG".into(),
            value,
            visibility: pc::EnvVisibility::Process,
        });
        let before = launch.env.clone();
        assert!(launch.project_instructions(delivery, TEXT).is_err(), "M2");
        assert_eq!(launch.env, before, "M2 no mutation");
    }
}

#[test]
fn file_routes_fail_closed_and_keep_a_retryable_pending_plan() {
    // Causes: C1 relative/braced path; C2 config list has wrong type; C3 no
    // materialized home. F1 C1=>error, pending plan retained; F2 C2=>error;
    // F3 C3=>explicit first-prompt placement; F4 valid retry=>one file/list.
    let delivery = crate::acp_cli("opencode").unwrap().instructions;
    for home in ["relative", "/session/{env:HOST}"] {
        let mut launch = empty_launch();
        launch.project_instructions(delivery, TEXT).unwrap();
        assert!(launch.bind_instructions_file(home).is_err(), "F1");
        assert_eq!(
            launch.instructions.placement(),
            InstructionsPlacement::FirstPrompt {
                reason: "session_home_unavailable"
            },
            "F3"
        );
        assert!(
            launch
                .bind_instructions_file("/session/valid")
                .unwrap()
                .is_some(),
            "F4"
        );
    }
    let mut launch = empty_launch();
    set_inline(
        &mut launch.env,
        "OPENCODE_CONFIG_CONTENT",
        r#"{"instructions":"wrong"}"#.into(),
    );
    launch.project_instructions(delivery, TEXT).unwrap();
    assert!(
        launch.bind_instructions_file("/session/valid").is_err(),
        "F2"
    );
}

#[test]
fn gemini_memory_imports_never_reinterpret_frozen_agent_text() {
    // Cause C1 an @path line would be interpreted as an import rather than
    // literal Agent instructions. G1 C1=>explicit FirstPrompt, no file read;
    // G2 ordinary text is covered by I3. Preserve semantics, not guessed escaping.
    let mut launch = empty_launch();
    launch
        .project_instructions(
            crate::acp_cli("gemini").unwrap().instructions,
            "rules\n@/host/private.md",
        )
        .unwrap();
    assert_eq!(
        launch.instructions.placement(),
        InstructionsPlacement::FirstPrompt {
            reason: "instruction_file_import_syntax"
        }
    );
    assert!(
        launch
            .bind_instructions_file("/session/private")
            .unwrap()
            .is_none()
    );
}

#[test]
fn placement_enriches_ready_without_repeating_lifecycle_or_prompt_content() {
    // Causes: O1 existing Launching/Ready phases; O2 instruction placement;
    // O3 existing Ready detail. Effects: exactly two phases, original scope,
    // route/reason only in Ready, and no frozen instruction content in events.
    // O1+O2+O3=>all effects; the real executor lifecycle test owns the call path.
    #[derive(Default)]
    struct Observer(std::sync::Mutex<Vec<(String, crate::AcpLaunchEvent)>>);
    impl crate::LaunchObserver for Observer {
        fn on_launch(&self, scope: &str, event: &crate::AcpLaunchEvent) {
            self.0.lock().unwrap().push((scope.into(), event.clone()));
        }
    }
    let target = Observer::default();
    let observer = InstructionsLaunchObserver::new(
        crate::LaunchSink::new(&target, "original-scope"),
        &AcpInstructions::default(),
    );
    crate::LaunchObserver::on_launch(
        &observer,
        "unused",
        &crate::AcpLaunchEvent::stage(crate::AcpLaunchStage::Launching),
    );
    crate::LaunchObserver::on_launch(
        &observer,
        "unused",
        &crate::AcpLaunchEvent::with_detail(crate::AcpLaunchStage::Ready, "session/prompt"),
    );
    let events = target.0.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|(scope, _)| scope == "original-scope"));
    assert_eq!(events[0].1.detail, None);
    assert_eq!(
        events[1].1.detail.as_deref(),
        Some("session/prompt instructions=first_prompt reason=uncataloged_launch")
    );
}

#[test]
fn system_delivery_omits_the_fallback_envelope_without_losing_request_context() {
    // Causes: C1 system route/fallback; C2 runtime context; C3 durable input.
    // P1 C1=system=>no duplicate frozen text; P2 C1=fallback=>one frozen block;
    // both C2+C3=>retain context/task. Durable input is byte-stable in both.
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    let mut activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run".into()),
        awaken_agent_contract::agent::thread::Id("thread".into()),
        awaken_runtime_contract::snapshot::ExecutableAgentSnapshot::builder("agent")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "provider",
                "model",
                "acp:claude",
            ))
            .build(),
        vec![Message::text(Id("user".into()), Role::User, "TASK")],
    );
    activation.snapshot.resolved_spec.instructions = "FROZEN_MARKER".into();
    let context = vec![Message::text(Id("env".into()), Role::System, "ENV_MARKER")];
    let durable = activation.input.clone();
    let mut launch = empty_launch();
    launch
        .project_instructions(
            crate::acp_cli("claude").unwrap().instructions,
            "FROZEN_MARKER",
        )
        .unwrap();
    for (instructions, count) in [(&launch.instructions, 0), (&AcpInstructions::default(), 1)] {
        let prompt = crate::initial_prompt(&activation, &context, instructions);
        assert_eq!(prompt.matches("FROZEN_MARKER").count(), count, "P1/P2");
        assert!(prompt.contains("ENV_MARKER") && prompt.contains("TASK"));
        assert_eq!(activation.input, durable);
    }
}
