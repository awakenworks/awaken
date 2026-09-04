use super::*;

#[test]
fn mcp_client_credential_admission_is_exact_and_has_no_gateway_fallback() {
    use awaken_credential_contract::McpCredentialDelivery;
    // Cause/effect graph: C1 delivery is exact ClientInjection; C2 the
    // selected catalog row declares its private field; C3 transport is
    // HTTP. Effect E1 admits credential material iff C1+C2+C3; otherwise
    // E2 rejects before launch. Constraint: all production MCP delivery is
    // the official ACP Session path, so no interface/fallback cause exists.
    // Decision rules: R1=111=>E1; R2=!C1 (None or Gateway)=>E2;
    // R3=1!1=>E2; R4=11!=>E2. These rules give MC/DC for each cause.
    for (delivery, declared, http, admitted) in [
        (
            Some(McpCredentialDelivery::ClientInjection),
            true,
            true,
            true,
        ),
        (
            Some(McpCredentialDelivery::ClientInjection),
            false,
            true,
            false,
        ),
        (
            Some(McpCredentialDelivery::ClientInjection),
            true,
            false,
            false,
        ),
        (None, true, true, false),
        (
            Some(McpCredentialDelivery::GatewayMediation),
            true,
            true,
            false,
        ),
    ] {
        assert_eq!(
            mcp_client_credential_admitted(delivery, declared, http),
            admitted
        );
    }
}

#[test]
fn catalog_datum_not_adapter_id_drives_process_private_mcp_auth() {
    use awaken_credential_contract::McpCredentialDelivery;

    // Causes: C1 one production row is selected; C2 its row datum declares
    // client injection; C3 its display id is relabeled independently.
    // Effects: E1 only declared rows admit; E2 an undeclared row rejects.
    // Constraint: identity is lookup metadata, never a capability allowlist.
    // Decision rules: R1=C1+C2=>E1 for every row; R2=C2+renamed=>E1;
    // R3=!C2+id "claude"=>E2, proving no hidden string branch remains.
    assert_eq!(
        known_acp_clis()
            .iter()
            .filter(|cli| {
                cli.admits_mcp_client_credential(Some(McpCredentialDelivery::ClientInjection), true)
            })
            .map(|cli| cli.id)
            .collect::<Vec<_>>(),
        ["claude"]
    );

    let mut renamed_declared = *acp_cli("claude").expect("declared row");
    renamed_declared.id = "fixture";
    assert!(
        renamed_declared
            .admits_mcp_client_credential(Some(McpCredentialDelivery::ClientInjection), true,)
    );

    let mut relabeled_undeclared = *acp_cli("codex").expect("undeclared row");
    relabeled_undeclared.id = "claude";
    assert!(
        !relabeled_undeclared
            .admits_mcp_client_credential(Some(McpCredentialDelivery::ClientInjection), true,)
    );
}

#[test]
fn each_executor_row_owns_its_model_api_dialect_capability() {
    // Causes: C1 selected ACP row; C2 offered API dialect. Effects: E1
    // compatible route; E2 fail-fast rejection. Decision rules cover every
    // production row and both matching/non-matching dialects, preventing a
    // second resolver-side adapter table from reappearing.
    let cases = [
        ("claude", "anthropic_messages"),
        ("codex", "open_ai_responses"),
        ("gemini", "gemini"),
        ("opencode", "open_ai_chat"),
        ("hermes", "open_ai_chat"),
    ];
    for (cli, dialect) in cases {
        let row = acp_cli(cli).unwrap();
        assert!(row.supports_model_api_dialect(dialect), "{cli}");
        assert!(!row.supports_model_api_dialect("unsupported"), "{cli}");
    }
}

#[test]
fn backend_owned_state_isolation_is_catalog_owned_and_codex_only() {
    // Cause/effect graph: C1 a backend-owned ACP row is selected; C2 the
    // external CLI exposes a state-only directory separate from its login
    // home. Effects: E1 Codex declares one exact Session-directory env;
    // E2 every other row shares host state; E3 relabeling a row cannot alter
    // the declared behavior. Constraint: the Runtime Host consumes this
    // datum and must not recreate an adapter-id allowlist.
    //
    // Decision table:
    // | rule | dedicated state dir | row      | effect |
    // | S1   | yes                 | Codex    | E1     |
    // | S2   | no                  | non-Codex| E2     |
    // | S3   | yes                 | relabeled| E3     |
    let codex = acp_cli("codex").expect("S1 Codex row");
    assert_eq!(
        codex.backend_owned_state_isolation,
        BackendOwnedStateIsolation::SessionDirectory {
            env: "CODEX_SQLITE_HOME"
        },
        "S1/E1"
    );
    assert!(
        known_acp_clis()
            .iter()
            .filter(|row| row.id != "codex")
            .all(|row| row.backend_owned_state_isolation == BackendOwnedStateIsolation::SharedHost),
        "S2/E2"
    );
    let mut relabeled = *codex;
    relabeled.id = "fixture";
    assert_eq!(
        relabeled.backend_owned_state_isolation, codex.backend_owned_state_isolation,
        "S3/E3"
    );
}

#[test]
fn publication_projection_preserves_each_catalog_rows_static_facts() {
    // Causes: C1 one executable catalog row; C2 it supports exact model
    // selection; C3 it exposes managed credential environments. Effects:
    // E1 one same-id publication capability; E2 exact-selection bit and E3
    // environment allowlist equal the row. Iterating every row is the
    // decision table and prevents a second hand-maintained adapter list.
    let projected = known_acp_publication_capabilities();
    assert_eq!(projected.len(), known_acp_clis().len());
    for row in known_acp_clis() {
        let capability = projected
            .iter()
            .find(|capability| capability.backend_ref == format!("acp:{}", row.id))
            .expect("E1");
        assert_eq!(
            capability.model_selection.admits_exact(),
            row.backend_model_interface != BackendModelInterface::Unsupported,
            "E2 {}",
            row.id
        );
        assert_eq!(
            capability.model_delivery_credential_environments,
            row.model_delivery.map(|delivery| {
                delivery
                    .credential_env
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect()
            }),
            "E3 {}",
            row.id
        );
    }
}

fn resolved() -> ResolvedModel {
    ResolvedModel::Managed {
        base_url: "https://api.minimaxi.com/anthropic".to_string(),
        model: "MiniMax-M3[1m]".to_string(),
        process_secret: Some(ProcessSecretRequirement::new("lease://test-model")),
        credential_artifact: None,
        acp: None,
        provider_server_tools: Vec::new(),
    }
}

fn env_of(launch: &AcpLaunch, key: &str) -> Option<String> {
    launch
        .env
        .iter()
        .find(|var| var.name == key)
        .and_then(|var| match &var.value {
            pc::EnvValue::Inline { value } => Some(value.clone()),
            pc::EnvValue::Secret { .. } => None,
        })
}

fn secret_ref_of<'a>(launch: &'a AcpLaunch, key: &str) -> Option<&'a str> {
    launch
        .env
        .iter()
        .find(|var| var.name == key)
        .and_then(|var| match &var.value {
            pc::EnvValue::Secret { reference } => Some(reference.as_str()),
            pc::EnvValue::Inline { .. } => None,
        })
}

#[test]
fn registry_holds_the_known_clis_and_unknown_fails_closed() {
    assert!(acp_cli("claude").is_some());
    assert!(acp_cli("codex").is_some());
    assert!(acp_cli("gemini").is_some());
    assert!(acp_cli("opencode").is_some());
    assert!(acp_cli("hermes").is_some());
    assert!(acp_cli("no_such_cli").is_none());
}

#[test]
fn managed_credential_delivery_is_catalog_data() {
    // Cause graph: catalog profile + pinned credential shape -> exactly one
    // managed delivery mechanism. Host code never reclassifies by CLI id.
    //
    // | Rule | Profile | Artifact | Process secret |
    // | C1 | Codex | auth.json | no |
    // | C2 | Claude | no | API key or setup-token env |
    // | C3 | Gemini/OpenCode | no | provider API-key env |
    let codex = acp_cli("codex").unwrap().managed_credential_delivery;
    let codex_artifact = codex.credential_artifact(false).expect("C1");
    assert_eq!(codex_artifact.relative_path, ".codex/auth.json", "C1");
    assert!(!codex.allows_process_secret(), "C1");

    let claude = acp_cli("claude").unwrap().managed_credential_delivery;
    assert_eq!(claude.credential_artifact(false), None, "C2");
    assert_eq!(claude.credential_artifact(true), None, "C2");
    assert!(claude.allows_process_secret(), "C2");
    let claude_model = acp_cli("claude").unwrap().model_delivery.unwrap();
    assert_eq!(
        claude_model.credential_env,
        &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        "C2"
    );

    for rule in ["gemini", "opencode"] {
        let delivery = acp_cli(rule).unwrap().managed_credential_delivery;
        assert_eq!(delivery.credential_artifact(false), None, "C3 {rule}");
        assert_eq!(delivery.credential_artifact(true), None, "C3 {rule}");
        assert!(delivery.allows_process_secret(), "C3 {rule}");
    }
}

#[test]
fn managed_model_interface_is_catalog_driven() {
    // Environment-based adapters retain their ordinary projection while a
    // protocol-owned adapter carries the exact id to the ACP Session. The
    // executor therefore needs no adapter-name branch.
    let route = resolved();
    for id in ["claude", "codex", "gemini", "opencode"] {
        assert_eq!(
            acp_cli(id).unwrap().managed_model_interface,
            ManagedModelInterface::Environment,
            "{id}"
        );
    }
    let hermes = acp_cli("hermes")
        .unwrap()
        .project(&route, Some(1_000_000), &[]);
    assert_eq!(hermes.session_model.as_deref(), Some("MiniMax-M3[1m]"));
    assert_eq!(
        env_of(&hermes, "HERMES_CONTEXT_WINDOW").as_deref(),
        Some("1000000"),
        "Hermes' offline startup metadata must retain the catalog window"
    );
}

#[test]
fn backend_owned_model_projection_is_catalog_driven_and_secret_free() {
    // Cause graph: BackendOwned policy -> one catalog model interface -> argv
    // or ACP Session option. Managed endpoint/key delivery is unreachable.
    //
    // Decision table:
    // B1 any known CLI + Default -> own default, no provider env/config option
    // B2 Claude + Exact         -> catalog config override
    // B3 Codex + Exact          -> ACP Session config option
    // B4 Gemini + Exact         -> catalog model flag
    // B5 OpenCode + Exact       -> fail closed, never default fallback
    for cli in known_acp_clis() {
        let mut stale_managed_env = vec![("HOME".into(), "/wrong-home".into())];
        if let Some(delivery) = cli.model_delivery {
            let credential_env = delivery
                .default_credential_env()
                .expect("catalog process-secret delivery has a default");
            stale_managed_env.extend([
                (delivery.base_url.into(), "https://wrong.invalid".into()),
                (delivery.model.into(), "wrong-model".into()),
                (credential_env.into(), "wrong-secret".into()),
            ]);
        }
        if let Some(config_home) = cli.config_home_env {
            stale_managed_env.push((config_home.into(), "/wrong-config".into()));
        }
        if let Some(delivery) = cli.managed_provider_config {
            stale_managed_env.push((delivery.config_env.into(), "wrong-provider-config".into()));
            if !delivery.provider_env.is_empty() {
                stale_managed_env.push((delivery.provider_env.into(), "wrong-provider".into()));
            }
        }
        let launch = cli
            .try_project(
                &ResolvedModel::backend_owned(
                    BackendModelSelection::Default,
                    "",
                    cli.id,
                    "test",
                    "sha256:test",
                    Default::default(),
                ),
                Some(999),
                &stale_managed_env,
            )
            .unwrap_or_else(|error| panic!("B1 {}: {error}", cli.id));
        assert!(launch.session_config_options.is_empty(), "B1 {}", cli.id);
        if let Some(delivery) = cli.model_delivery {
            for key in std::iter::once(delivery.base_url)
                .chain(std::iter::once(delivery.model))
                .chain(delivery.credential_env.iter().copied())
                .chain(delivery.aliases.iter().copied())
            {
                assert!(env_of(&launch, key).is_none(), "B1 {} leaked {key}", cli.id);
            }
        }
        if let Some(config_home) = cli.config_home_env {
            assert!(env_of(&launch, config_home).is_none(), "B1 {}", cli.id);
        }
        if let Some(delivery) = cli.managed_provider_config {
            assert!(
                env_of(&launch, delivery.config_env).is_none(),
                "B1 {}",
                cli.id
            );
            if !delivery.provider_env.is_empty() {
                assert!(
                    env_of(&launch, delivery.provider_env).is_none(),
                    "B1 {}",
                    cli.id
                );
            }
        }
    }

    let exact = ResolvedModel::backend_owned(
        BackendModelSelection::Exact,
        "model-x",
        "codex",
        "test",
        "sha256:test",
        Default::default(),
    );
    let claude = acp_cli("claude")
        .unwrap()
        .try_project(&exact, None, &[])
        .unwrap();
    assert!(
        claude
            .argv
            .ends_with(&["-c".into(), "model=\"model-x\"".into()]),
        "B2"
    );

    let codex = acp_cli("codex")
        .unwrap()
        .try_project(&exact, None, &[])
        .unwrap();
    assert_eq!(
        codex.session_config_options,
        vec![awaken_protocol_acp::SessionConfigOptionSelection {
            config_id: "model".into(),
            value: "model-x".into(),
        }],
        "B3"
    );

    let gemini = acp_cli("gemini")
        .unwrap()
        .try_project(&exact, None, &[])
        .unwrap();
    assert!(
        gemini.argv.ends_with(&["--model".into(), "model-x".into()]),
        "B4"
    );

    let error = acp_cli("opencode")
        .unwrap()
        .try_project(&exact, None, &[])
        .unwrap_err();
    assert_eq!(error.0, "backend_exact_model_unsupported: opencode", "B5");
}

// ── Property tests over EVERY catalog row ────────────────────────────────────
// These hold for every current and future CLI, so adding a row (opencode, …) is
// covered by construction — the invariants a new agent must satisfy, not a
// per-agent copy of the same assertions.

#[test]
fn every_cli_has_a_unique_id_and_a_nonempty_launch() {
    let mut ids: Vec<&str> = known_acp_clis().iter().map(|c| c.id).collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n, "catalog has a duplicate CLI id");
    for cli in known_acp_clis() {
        assert!(
            !cli.acquisition.executable().is_empty(),
            "{}: acquisition executable is set",
            cli.id
        );
        assert!(
            !cli.discovery.version.executable.is_empty()
                && !cli.discovery.login.command.executable.is_empty()
                && !cli.discovery.login.rules.is_empty(),
            "{}: discovery is complete",
            cli.id
        );
        assert!(
            cli.discovery
                .login
                .rules
                .iter()
                .any(|rule| { rule.state == CredentialObservationState::Available }),
            "{}: login rules can prove availability",
            cli.id
        );
        assert!(
            cli.model_api_dialects.is_empty() || cli.model_delivery.is_some(),
            "{}: an advertised Provider dialect has endpoint/model delivery",
            cli.id
        );
    }
}

#[test]
fn every_cli_injects_the_resolved_model_base_url_and_secret_key() {
    let m = resolved();
    let ResolvedModel::Managed {
        base_url, model, ..
    } = &m
    else {
        unreachable!()
    };
    for cli in known_acp_clis() {
        if !cli.managed_credential_delivery.allows_process_secret() {
            assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
            continue;
        }
        let Some(d) = cli.model_delivery.as_ref() else {
            assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
            continue;
        };
        let launch = cli.project(&m, None, &[]);
        assert_eq!(
            env_of(&launch, d.base_url).as_deref(),
            Some(base_url.as_str()),
            "{}: base_url",
            cli.id
        );
        assert_eq!(
            env_of(&launch, d.model).as_deref(),
            Some(model.as_str()),
            "{}: model",
            cli.id
        );
        assert_eq!(
            secret_ref_of(
                &launch,
                d.default_credential_env()
                    .expect("catalog process-secret delivery has a default"),
            ),
            Some("lease://test-model"),
            "{}: key",
            cli.id
        );
        for alias in d.aliases {
            assert_eq!(
                env_of(&launch, alias).as_deref(),
                Some(model.as_str()),
                "{}: alias {alias}",
                cli.id
            );
        }
    }
}

#[test]
fn every_cli_keeps_the_secret_unshadowable_by_passthrough() {
    let m = resolved();
    let ResolvedModel::Managed { model, .. } = &m else {
        unreachable!()
    };
    for cli in known_acp_clis() {
        if !cli.managed_credential_delivery.allows_process_secret() {
            assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
            continue;
        }
        let Some(d) = cli.model_delivery else {
            assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
            continue;
        };
        let credential_env = d
            .default_credential_env()
            .expect("catalog process-secret delivery has a default");
        // A hostile passthrough tries to override the modeled model + the secret.
        let extra = vec![
            (d.model.to_string(), "attacker-model".to_string()),
            (credential_env.to_string(), "attacker-key".to_string()),
        ];
        let launch = cli.project(&m, None, &extra);
        assert_eq!(
            env_of(&launch, d.model).as_deref(),
            Some(model.as_str()),
            "{}: typed model wins",
            cli.id
        );
        assert_eq!(
            secret_ref_of(&launch, credential_env),
            Some("lease://test-model"),
            "{}: secret unshadowable",
            cli.id
        );
    }
}

#[test]
fn claude_setup_token_uses_only_its_allowlisted_process_environment() {
    let model = ResolvedModel::Managed {
        base_url: "https://api.anthropic.com/v1".into(),
        model: "claude-test".into(),
        process_secret: Some(ProcessSecretRequirement::for_environment(
            "lease://setup-token",
            "CLAUDE_CODE_OAUTH_TOKEN",
        )),
        credential_artifact: None,
        acp: None,
        provider_server_tools: Vec::new(),
    };
    let claude = acp_cli("claude").unwrap();
    let launch = claude
        .try_project(&model, None, &[])
        .expect("Claude setup token");
    assert_eq!(
        secret_ref_of(&launch, "CLAUDE_CODE_OAUTH_TOKEN"),
        Some("lease://setup-token")
    );
    assert_eq!(secret_ref_of(&launch, "ANTHROPIC_API_KEY"), None);

    let gemini = acp_cli("gemini").unwrap();
    let error = gemini
        .try_project(&model, None, &[])
        .expect_err("another CLI must reject Claude's setup token");
    assert!(error.0.contains("credential_environment_unsupported"));
}

#[test]
fn every_cli_egresses_through_the_gateway_with_a_lease_never_a_raw_key() {
    // D-R2 for the whole catalog: no matter which CLI runs in the sandbox, a
    // cloud-managed launch carries only a lease requirement, never a raw provider key.
    let raw = "sk-RAW-PROVIDER-SECRET"; // awaken-allow: secret
    for cli in known_acp_clis() {
        if !cli.managed_credential_delivery.allows_process_secret() {
            assert!(
                cli.try_project(
                    &ResolvedModel::cloud_managed_gateway(
                        "https://gateway.awaken.internal",
                        "some-model",
                        "lease://gateway-123",
                    ),
                    None,
                    &[],
                )
                .is_err(),
                "{}: artifact-only adapter rejects a process lease",
                cli.id
            );
            continue;
        }
        let Some(delivery) = cli.model_delivery else {
            assert!(
                cli.try_project(&resolved(), None, &[]).is_err(),
                "{}",
                cli.id
            );
            continue;
        };
        let model = ResolvedModel::cloud_managed_gateway(
            "https://gateway.awaken.internal",
            "some-model",
            "lease://gateway-123",
        );
        let launch = cli.project(&model, None, &[]);
        assert!(
            !format!("{launch:?}").contains(raw),
            "{}: raw key must never appear",
            cli.id
        );
        assert_eq!(
            secret_ref_of(
                &launch,
                delivery
                    .default_credential_env()
                    .expect("catalog process-secret delivery has a default"),
            ),
            Some("lease://gateway-123"),
            "{}: key env holds only the broker reference",
            cli.id
        );
        assert_eq!(
            env_of(&launch, delivery.base_url).as_deref(),
            Some("https://gateway.awaken.internal"),
            "{}: base_url is the gateway",
            cli.id
        );
    }
}

#[test]
fn every_local_dir_cli_declares_a_subtree_and_never_harvests_its_credentials() {
    // A LocalDir CLI must name a harvestable session subtree; its credentials
    // (session_export_excludes) must live OUTSIDE that subtree, so a cross-machine session
    // blob never carries an auth file.
    for cli in known_acp_clis() {
        if let SessionPersistence::LocalDir {
            session_subpath, ..
        } = cli.session_persistence
        {
            assert!(
                !session_subpath.is_empty(),
                "{}: LocalDir needs a session subpath",
                cli.id
            );
            for cred in cli.session_export_excludes {
                assert!(
                    !cred.starts_with(session_subpath),
                    "{}: credential {cred} must not live under the harvested session subtree {session_subpath}",
                    cli.id
                );
            }
        }
    }
}

#[test]
fn opencode_resolves_and_projects_like_a_native_openai_compatible_cli() {
    let cli = acp_cli("opencode").unwrap();
    let launch = cli.project(&resolved(), None, &[]);
    assert_eq!(
        env_of(&launch, "OPENAI_BASE_URL").as_deref(),
        Some("https://api.minimaxi.com/anthropic")
    );
    assert_eq!(
        secret_ref_of(&launch, "OPENAI_API_KEY"),
        Some("lease://test-model")
    );
    let config = env_of(&launch, "OPENCODE_CONFIG_CONTENT")
        .expect("OpenCode managed launches require an explicit provider document");
    let config: serde_json::Value =
        serde_json::from_str(&config).expect("valid OpenCode provider config");
    assert_eq!(config["model"], "awaken-managed/MiniMax-M3[1m]");
    assert_eq!(
        config["provider"]["awaken-managed"]["npm"],
        "@ai-sdk/openai-compatible"
    );
    assert_eq!(
        config["provider"]["awaken-managed"]["options"]["baseURL"],
        "https://api.minimaxi.com/anthropic"
    );
    assert_eq!(
        config["provider"]["awaken-managed"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
    assert_eq!(
        config["provider"]["awaken-managed"]["models"]["MiniMax-M3[1m]"]["name"],
        "MiniMax-M3[1m]"
    );
    assert!(!config.to_string().contains("lease://test-model"));
    assert_eq!(
        env_of(&launch, "OPENCODE_DISABLE_MODELS_FETCH").as_deref(),
        Some("true")
    );
}

#[test]
fn session_persistence_is_declared_per_cli_with_the_right_keying() {
    // Claude keys sessions by cwd (recovery needs a stable interior cwd); Codex
    // keys by an internal id (cwd-independent). Both are LocalDir → harvestable.
    assert_eq!(
        acp_cli("claude").unwrap().session_persistence,
        SessionPersistence::LocalDir {
            session_subpath: "projects",
            keyed_by: SessionKey::Cwd,
        }
    );
    assert_eq!(
        acp_cli("codex").unwrap().session_persistence,
        SessionPersistence::None
    );
}

#[test]
fn cloud_managed_gateway_puts_a_lease_requirement_not_a_raw_key_in_the_plan() {
    // D-R2: an ACP CLI in the untrusted sandbox must egress through the gateway
    // with a brokered lease token, never a raw provider key.
    let raw_provider_key = "sk-REAL-PROVIDER-SECRET"; // awaken-allow: secret
    let model = ResolvedModel::cloud_managed_gateway(
        "https://gateway.awaken.internal",
        "claude-opus-4-8",
        "lease://gateway-abc123",
    );
    let cli = acp_cli("claude").unwrap();
    let launch = cli.project(&model, None, &[]);

    // The CLI's base_url is the gateway and its key requirement is opaque.
    assert_eq!(
        env_of(&launch, "ANTHROPIC_BASE_URL").as_deref(),
        Some("https://gateway.awaken.internal")
    );
    assert_eq!(
        secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
        Some("lease://gateway-abc123")
    );
    // The raw provider key never appears in any launch env value.
    assert!(
        !format!("{launch:?}").contains(raw_provider_key),
        "raw provider key must never enter the ACP launch env"
    );
}

#[test]
fn acquisition_kind_is_the_single_source_for_preinstalled_argv_and_acquisition_phase() {
    // Acquisition cause graph:
    // catalog kind ──> exact local argv ──> launch + discovery executable
    //              └─> startup acquisition requirement
    //
    // Decision table:
    // D1 pinned wrapper | preinstalled bin | startup install required
    // D2 direct native  | executable,args  | no startup install
    let cases: [(&str, &[&str], bool); 4] = [
        ("claude", &["claude-agent-acp"], true),
        ("codex", &["codex-acp"], true),
        ("gemini", &["gemini", "--acp"], false),
        ("opencode", &["opencode", "acp"], false),
    ];

    for (id, expected_argv, expected_install) in cases {
        let acquisition = acp_cli(id).unwrap().acquisition;
        assert_eq!(acquisition.executable(), expected_argv[0], "{id}");
        assert_eq!(acquisition.local_argv(), expected_argv, "{id}");
        assert_eq!(
            acquisition.requires_installation(),
            expected_install,
            "{id}"
        );
        assert!(
            acquisition
                .local_argv()
                .iter()
                .all(|arg| !arg.ends_with("@latest")),
            "{id}: acquisition must be reproducibly pinned"
        );
    }
}

#[test]
fn capability_probe_credentials_never_change_the_agent_launch_argv() {
    let cli = acp_cli("codex").unwrap();
    assert_eq!(cli.container_argv, ["codex-acp"]);
    assert_eq!(
        cli.container_probe_argv.unwrap(),
        [
            "/usr/bin/env",
            "OPENAI_API_KEY=awaken-capability-probe",
            "codex-acp"
        ]
    );
    assert_eq!(cli.capability_probe_auth_method_id, Some("api-key"));
    assert_eq!(cli.auth_method_id, None);

    let hermes = acp_cli("hermes").unwrap();
    assert_eq!(hermes.container_argv, ["hermes-acp"]);
    assert_eq!(
        hermes.container_probe_argv.unwrap(),
        [
            "/usr/bin/env",
            "DEEPSEEK_API_KEY=awaken-capability-probe",
            "DEEPSEEK_BASE_URL=http://127.0.0.1:9/v1",
            "HERMES_MODEL=deepseek-v4-flash",
            "hermes-acp"
        ],
        "a prompt-free capability probe must not depend on provider egress"
    );
    assert_eq!(
        hermes.model_delivery.unwrap().base_url,
        "DEEPSEEK_BASE_URL",
        "the real managed route remains runtime-provisioned"
    );

    for cli in known_acp_clis() {
        for argument in cli.container_probe_argv.unwrap_or_default() {
            if argument.contains("_API_KEY=") {
                assert!(
                    argument.ends_with("=awaken-capability-probe"),
                    "{} probe must contain only the public sentinel",
                    cli.id
                );
            }
        }
    }
}

#[test]
fn wrapper_catalog_uses_exact_packages_and_resolved_argv_preserves_model_delivery() {
    // Cause graph: exact catalog package -> startup-resolved absolute argv ->
    // canonical model projection. The override replaces acquisition only;
    // it cannot replace model, MCP, environment, or credential policy.
    //
    // Decision table:
    // W1 wrapper catalog row -> exact x.y.z package and stable bin
    // W2 default model       -> absolute argv, no model override
    // W3 exact model         -> absolute argv + catalog model interface
    for id in ["claude", "codex"] {
        let AcpAcquisition::PinnedNpmWrapper { package, bin, .. } =
            acp_cli(id).unwrap().acquisition
        else {
            panic!("W1 {id}");
        };
        let (_, version) = package.rsplit_once('@').expect("W1 exact package");
        assert_eq!(
            version.split('.').count(),
            3,
            "W1 {id}: package must pin x.y.z"
        );
        assert!(version.split('.').all(|part| part.parse::<u64>().is_ok()));
        assert!(!bin.trim().is_empty(), "W1 {id}");
    }

    let claude = acp_cli("claude").unwrap();
    let absolute = vec!["/var/lib/awaken/acp-wrappers/claude-agent-acp".to_string()];
    let default = claude
        .try_project_with_argv(
            &ResolvedModel::backend_owned(
                BackendModelSelection::Default,
                "",
                "codex",
                "test",
                "sha256:test",
                Default::default(),
            ),
            None,
            &[],
            Some(&absolute),
        )
        .expect("W2");
    assert_eq!(default.argv, absolute, "W2");

    let exact = claude
        .try_project_with_argv(
            &ResolvedModel::backend_owned(
                BackendModelSelection::Exact,
                "model-x",
                "codex",
                "test",
                "sha256:test",
                Default::default(),
            ),
            None,
            &[],
            Some(&absolute),
        )
        .expect("W3");
    assert_eq!(
        exact.argv,
        [absolute[0].clone(), "-c".into(), "model=\"model-x\"".into()],
        "W3"
    );
    assert!(
        claude
            .try_project_with_argv(
                &ResolvedModel::backend_owned(
                    BackendModelSelection::Default,
                    "",
                    "codex",
                    "test",
                    "sha256:test",
                    Default::default(),
                ),
                None,
                &[],
                Some(&[]),
            )
            .is_err(),
        "empty acquisition evidence fails closed"
    );
}

#[test]
fn managed_credential_delivery_mismatch_fails_closed_in_both_directions() {
    // Causes: C1 CLI profile selects artifact/process delivery; C2 launch
    // supplies the opposite credential shape. Effects: E1 model delivery
    // remains available; E2 both mismatches fail before process launch.
    //
    // | Rule | CLI delivery | supplied | Effect |
    // | D1 | artifact | process | E2 |
    // | D2 | process | artifact | E2 |
    let cli = acp_cli("codex").unwrap();
    assert!(cli.model_delivery.is_some(), "E1");
    assert!(cli.config_home_env.is_none());
    assert!(cli.session_export_excludes.is_empty());
    let error = cli.try_project(&resolved(), None, &[]).unwrap_err();
    assert_eq!(
        error.0, "credential_delivery_mismatch: codex requires a managed artifact",
        "D1"
    );
    let claude = acp_cli("claude").unwrap();
    let artifact = ResolvedModel::managed(
        "https://anthropic.example/v1",
        "claude-test",
        None,
        Some(CredentialArtifactRequirement::new(
            "credential://artifact",
            ".config/credential.json",
        )),
    );
    assert_eq!(
        claude.try_project(&artifact, None, &[]).unwrap_err().0,
        "credential_delivery_mismatch: claude requires a process secret",
        "D2"
    );
}

#[test]
fn codex_artifact_launch_projects_model_but_not_credential_environment() {
    // C1 exact endpoint/model -> E1 generic coordinates plus the adapter's
    // selected managed-provider config. C2 artifact-backed credential -> E2
    // no plaintext credential environment or config value.
    let cli = acp_cli("codex").unwrap();
    let model = ResolvedModel::managed(
        "https://openai-compatible.example/v1",
        "qwen/qwen3-235b",
        None,
        Some(CredentialArtifactRequirement::new(
            "awaken-credential-artifact://one-shot",
            ".codex/auth.json",
        )),
    );
    let launch = cli.try_project(&model, None, &[]).expect("artifact launch");
    assert_eq!(
        env_of(&launch, "OPENAI_BASE_URL").as_deref(),
        Some("https://openai-compatible.example/v1"),
        "E1"
    );
    assert_eq!(
        env_of(&launch, "OPENAI_MODEL").as_deref(),
        Some("qwen/qwen3-235b"),
        "E1"
    );
    assert_eq!(secret_ref_of(&launch, "OPENAI_API_KEY"), None, "E2");
    assert_eq!(
        env_of(&launch, "MODEL_PROVIDER").as_deref(),
        Some("awaken-managed"),
        "E1"
    );
    let config = env_of(&launch, "CODEX_CONFIG").expect("managed provider config");
    let config: serde_json::Value =
        serde_json::from_str(&config).expect("valid managed provider config");
    assert_eq!(config["model"], "qwen/qwen3-235b", "E1");
    assert_eq!(config["model_provider"], "awaken-managed", "E1");
    assert_eq!(
        config["model_providers"]["awaken-managed"]["base_url"],
        "https://openai-compatible.example/v1",
        "E1"
    );
    assert_eq!(
        config["model_providers"]["awaken-managed"]["wire_api"], "responses",
        "E1"
    );
    assert_eq!(
        config["model_providers"]["awaken-managed"]["requires_openai_auth"], true,
        "E1"
    );
    assert_eq!(
        config["model_providers"]["awaken-managed"]["supports_standalone_web_search"], false,
        "WebSearch is never enabled by protocol compatibility alone"
    );
    assert!(config.get("web_search").is_none());
    assert!(!config.to_string().contains("one-shot"), "E2");
}

#[test]
fn codex_native_web_search_requires_and_projects_the_exact_responses_capability() {
    use awaken_runtime_contract::resolved::ProviderServerTool;

    let codex = acp_cli("codex").unwrap();
    for projection in [
        ProviderServerTool::OpenAiWebSearch,
        ProviderServerTool::DeepSeekResponsesWebSearch,
    ] {
        let provider = projection.provider_kind();
        codex
            .validate_provider_server_tool_route(
                provider,
                "open_ai_responses",
                std::slice::from_ref(&projection),
            )
            .expect("exact provider and Responses dialect");
        let model = ResolvedModel::managed(
            "https://responses.example/v1",
            "model",
            None,
            Some(CredentialArtifactRequirement::new(
                "credential://search",
                ".codex/auth.json",
            )),
        )
        .with_provider_server_tools([projection]);
        let launch = codex
            .try_project(&model, None, &[])
            .expect("exact Codex Responses WebSearch projection");
        let config: serde_json::Value =
            serde_json::from_str(&env_of(&launch, "CODEX_CONFIG").expect("Codex config"))
                .expect("valid Codex config");
        assert_eq!(config["web_search"], "live");
        assert_eq!(
            config["model_providers"]["awaken-managed"]["supports_standalone_web_search"],
            true
        );
    }

    for (provider, dialect, projection, error) in [
        (
            "openai",
            "open_ai_responses",
            ProviderServerTool::DeepSeekResponsesWebSearch,
            "provider_server_tool_mismatch",
        ),
        (
            "openai",
            "open_ai_chat",
            ProviderServerTool::OpenAiWebSearch,
            "provider_server_tool_dialect_mismatch",
        ),
        (
            "anthropic",
            "anthropic_messages",
            ProviderServerTool::AnthropicWebSearch,
            "provider_server_tool_unsupported",
        ),
    ] {
        let actual = codex
            .validate_provider_server_tool_route(provider, dialect, &[projection])
            .unwrap_err();
        assert!(actual.0.starts_with(error), "{actual}");
    }
    codex
        .validate_provider_server_tool_route("openai", "open_ai_chat", &[])
        .expect("no selected provider tool leaves the model route untouched");

    for (cli, projection) in [
        ("codex", ProviderServerTool::AnthropicWebSearch),
        ("claude", ProviderServerTool::AnthropicWebSearch),
        ("gemini", ProviderServerTool::GeminiWebSearch),
    ] {
        let adapter = acp_cli(cli).unwrap();
        let model = ResolvedModel::managed(
            "https://provider.example/v1",
            "model",
            Some(ProcessSecretRequirement::new("credential://search")),
            None,
        )
        .with_provider_server_tools([projection]);
        let error = adapter.try_project(&model, None, &[]).unwrap_err();
        assert!(
            error.0.starts_with("provider_server_tool_unsupported:"),
            "{cli}: {error}"
        );
    }
}

#[test]
fn provider_managed_acp_launch_projects_the_publication_pinned_session_profile() {
    // Causes: C1 provider-managed coordinates; C2 artifact credential; C3
    // publication-pinned ACP mode/options/capability. Effects: E1 model env;
    // E2 exact native Session config; E3 handshake expectation uses the
    // frozen adapter version/fingerprint.
    let cli = acp_cli("codex").unwrap();
    let model = ResolvedModel::managed_with_acp(
        "https://openai-compatible.example/v1",
        "qwen/qwen3-235b",
        None,
        Some(CredentialArtifactRequirement::new(
            "awaken-credential-artifact://one-shot",
            ".codex/auth.json",
        )),
        awaken_runtime_contract::resolved::AcpExecutionProfile {
            capability_adapter_version: "1.2.3".into(),
            capability_fingerprint: "sha256:profile".into(),
            session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration {
                mode: Some("plan".into()),
                options: [("reasoning_effort".into(), "high".into())]
                    .into_iter()
                    .collect(),
                working_directory: Some("repo/src".into()),
            },
        },
    );
    let launch = cli
        .try_project(&model, None, &[])
        .expect("published profile");
    assert_eq!(launch.session_mode.as_deref(), Some("plan"), "E2");
    assert_eq!(launch.session_config_options.len(), 1, "E2");
    assert_eq!(
        launch.session_config_options[0].config_id, "reasoning_effort",
        "E2"
    );
    assert_eq!(launch.session_config_options[0].value, "high", "E2");
    assert_eq!(
        launch.session_working_directory.as_deref(),
        Some("repo/src"),
        "E2"
    );
    let expectation = launch.expected_capability.expect("E3");
    assert_eq!(expectation.adapter_id, "codex", "E3");
    assert_eq!(expectation.adapter_version, "1.2.3", "E3");
    assert_eq!(expectation.fingerprint, "sha256:profile", "E3");
}

#[test]
fn launch_projection_rejects_a_stale_unsafe_working_directory() {
    let model = ResolvedModel::managed_with_acp(
        "https://provider.example/v1",
        "model",
        None,
        Some(CredentialArtifactRequirement::new(
            "awaken-credential-artifact://one-shot",
            ".codex/auth.json",
        )),
        awaken_runtime_contract::resolved::AcpExecutionProfile {
            capability_adapter_version: "1".into(),
            capability_fingerprint: "sha256:stale".into(),
            session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration {
                working_directory: Some("../outside".into()),
                ..Default::default()
            },
        },
    );
    let error = acp_cli("codex")
        .unwrap()
        .try_project(&model, None, &[])
        .unwrap_err();
    assert!(error.0.starts_with("invalid ACP working directory:"));
}

#[test]
fn claude_projects_model_base_url_tier_aliases_and_compact_window() {
    // Launch-projection cause graph (the catalog is the sole constructor):
    // C1 catalog row has model delivery + C2 resolved coordinates
    //   -> E1 catalog argv + E2 typed model env + E3 optional window env.
    // A CLI without model delivery instead requires its typed artifact driver;
    // no adapter-specific AcpLaunch constructor can bypass these branches.
    //
    // Decision table (covered by this test and the Codex tests above):
    // | Rule | delivery | artifact | coordinates | result |
    // | L1 | present | - | present | catalog argv + typed env |
    // | L2 | absent | absent | any | fail closed |
    // | L3 | absent | present | any | catalog argv, no provider env |
    let cli = acp_cli("claude").unwrap();
    let launch = cli.project(&resolved(), Some(1_000_000), &[]);

    assert_eq!(launch.argv, vec!["claude-agent-acp"]);
    assert_eq!(
        env_of(&launch, "ANTHROPIC_BASE_URL").as_deref(),
        Some("https://api.minimaxi.com/anthropic")
    );
    assert_eq!(
        env_of(&launch, "ANTHROPIC_MODEL").as_deref(),
        Some("MiniMax-M3[1m]")
    );
    // Tier aliases all carry the same resolved model.
    for alias in [
        "ANTHROPIC_SONNET_MODEL",
        "ANTHROPIC_OPUS_MODEL",
        "ANTHROPIC_HAIKU_MODEL",
    ] {
        assert_eq!(env_of(&launch, alias).as_deref(), Some("MiniMax-M3[1m]"));
    }
    // Compact window from the run's context config.
    assert_eq!(
        env_of(&launch, "CLAUDE_CODE_AUTO_COMPACT_WINDOW").as_deref(),
        Some("1000000")
    );
    // Projection carries only the one-shot requirement. Materialization is
    // deferred until the concrete process boundary.
    assert_eq!(
        secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
        Some("lease://test-model")
    );
}

#[test]
fn typed_model_delivery_wins_over_passthrough_and_secret_is_unshadowable() {
    let cli = acp_cli("claude").unwrap();
    // A stray passthrough tries to override the modeled model + the secret.
    let extra = vec![
        ("ANTHROPIC_MODEL".to_string(), "attacker-model".to_string()),
        ("ANTHROPIC_API_KEY".to_string(), "attacker-key".to_string()),
        ("EXTRA_FLAG".to_string(), "1".to_string()),
    ];
    let launch = cli.project(&resolved(), None, &extra);
    // Typed model wins; the opaque secret requirement cannot be shadowed by
    // ordinary env; unmodeled passthrough survives.
    assert_eq!(
        env_of(&launch, "ANTHROPIC_MODEL").as_deref(),
        Some("MiniMax-M3[1m]")
    );
    assert_eq!(
        secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
        Some("lease://test-model")
    );
    assert_eq!(env_of(&launch, "EXTRA_FLAG").as_deref(), Some("1"));
}
