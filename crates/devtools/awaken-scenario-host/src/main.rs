//! `awaken-coordinator` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised; `=vision` reports the media it
//! received so the multimodal path can be verified. A provider executor is
//! swapped in through [`awaken_scenario_host::build_router`].

use std::sync::Arc;

/// Deterministic list prices for the existing management Scenario composition.
///
/// Cause/effect table: P1 no provider + budget -> fail closed (covered by the
/// production composition); P2 this explicit Scenario provider + every frozen
/// model ref -> one complete immutable snapshot; P3 an unlisted model cannot
/// occur because the snapshot is derived from the exact requested roster. A
/// two-token Scenario response costs exactly one public cent, making the real
/// budget gate observable without installing a production default price.
struct ScenarioListPriceProvider;

#[async_trait::async_trait]
impl awaken_session_contract::ManagedListPriceProvider for ScenarioListPriceProvider {
    async fn resolve_snapshot(
        &self,
        request: awaken_session_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_contract::ManagedListPriceSnapshot,
        awaken_session_contract::ManagedListPriceError,
    > {
        let rates = awaken_session_contract::ManagedTokenListRates {
            input_micros_per_million: 5_000_000_000,
            output_micros_per_million: 5_000_000_000,
            cache_read_micros_per_million: 5_000_000_000,
            cache_creation_micros_per_million: 5_000_000_000,
        };
        Ok(awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "scenario-list-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: request.occurred_at_unix_ms,
            arithmetic_version: 1,
            model_rates: request
                .model_refs
                .into_iter()
                .map(|model_ref| (model_ref, rates))
                .collect(),
            runtime_rates: Default::default(),
            fingerprint: "scenario-list-prices-v1".into(),
        })
    }
}

fn management_scenario_adapters(
    rate_limit_transition: bool,
) -> awaken_process_startup::ManagedServiceAdapters {
    let adapters = awaken_process_startup::ManagedServiceAdapters::default()
        .with_list_price_provider(Arc::new(ScenarioListPriceProvider))
        .with_tunnel_application(Arc::new(
            awaken_protocol_managed_testkit::EmptyManagedTunnelApplication,
        ));
    if !rate_limit_transition {
        return adapters;
    }
    adapters.with_request_limiter(Arc::new(awaken_authz_enforce::OrgRateLimiter::with_limits(
        "scenario-organization",
        awaken_tenancy::org_admission::OrgRateLimits {
            create_per_minute: 3,
            read_per_minute: 1_200,
        },
        awaken_time::system_clock(),
    )))
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_process_startup::block_on_service(async_main)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the scenario harness is its own process root and reads its scenario configuration from the environment"
)]
async fn async_main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let model_mode = std::env::var("AWAKEN_MODEL_MODE");
    // The scenario-only role axis (`AWAKEN_SCENARIO_ROLE`). Worker is an execution
    // endpoint that never starts the HTTP surface; Serve is the default —
    // single-machine all-in-one, or a coordinator when the local pool is disabled.
    // The hand is the separate `awaken-sandbox hand` execution-plane binary.
    if std::env::var("AWAKEN_SCENARIO_ROLE").as_deref() == Ok("worker") {
        let observability = scenario_observability();
        awaken_observability::init(&observability);
        let trace_file_present = observability.trace_file.is_some();
        let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
        let worker_id = std::env::var("AWAKEN_WORKER_ID")
            .unwrap_or_else(|_| "awaken-scenario-worker".to_string());
        let admin_listen = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")
            .ok()
            .filter(|address| !address.trim().is_empty());
        // The echo Worker process's one wall clock: its request assertions and
        // the Host it builds share it.
        let clock = awaken_time::system_clock();
        let request_authorizer = std::env::var("AWAKEN_WORKER_REQUEST_CREDENTIAL_FILE")
            .ok()
            .map(|path| {
                awaken_worker_transport_security::load_projected_request_authorizer(
                    std::path::Path::new(&path),
                    &worker_id,
                    clock.clone(),
                )
            })
            .transpose()?;
        // Test-only echo-draining worker (the worker-pool e2e). The production
        // worker with real per-run model resolution lives in `awaken-worker`.
        return awaken_scenario_host::run_echo_worker(
            &upstream,
            &worker_id,
            admin_listen.as_deref(),
            request_authorizer,
            clock,
            trace_file_present,
        )
        .await;
    }
    if model_mode.as_deref() == Ok("distributed-control") {
        return awaken_scenario_host::serve_distributed_control(awaken_time::system_clock())
            .await
            .map_err(std::io::Error::other)
            .map_err(Into::into);
    }
    // Scenario environment is test-harness input, translated here into the same
    // typed policy production receives from config.toml. The observability adapter
    // itself has no ambient compatibility path. Split Control owns this lifecycle
    // inside `serve_prepared_control` and returned above before a second init.
    awaken_scenario_host::realization_observation::install_from_environment();
    let observability = scenario_observability();
    awaken_observability::init(&observability);
    let trace_file_present = observability.trace_file.is_some();
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    let uses_shared_runtime = mode_uses_shared_scenario_runtime(model_mode.as_deref().ok());
    let mut scenario_lifecycle =
        uses_shared_runtime.then(awaken_service_lifecycle::ServiceLifecycle::new);
    if uses_shared_runtime {
        // Durability guard: refuse to boot a `typed durable ingress` ingress that would
        // resolve to a volatile in-memory queue (durable + default sqlite backend + no
        // DeploymentConfig::storage_dir) — such a queue silently drops every queued/crashed/scheduled
        // run on restart, defeating the whole point of durable ingress (no-data-loss).
        let deployment = awaken_scenario_host::scenario_deployment();
        if let Some(error) = deployment.durable_needs_persistence_error(false) {
            return Err(error.to_owned().into());
        }
        // Host-backed scenarios share one explicit test-support runtime authority.
        // AllInOne and split-Control modes assemble their own canonical authority;
        // installing this one as well would create a redundant, unused owner.
        let runtime_authority = awaken_process_startup::scenario_runtime_authority(
            &deployment.coordinator_storage(),
            awaken_time::system_clock(),
        )
        .await?;
        awaken_scenario_host::install_scenario_runtime_authority(runtime_authority)?;
    }
    let app = match model_mode.as_deref() {
        Ok("probe") => {
            awaken_scenario_host::build_probe_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("revise") => {
            awaken_scenario_host::build_router(
                Arc::new(awaken_scenario_host::ReviseModel),
                "revise",
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("outcome-matrix") => {
            awaken_scenario_host::build_outcome_matrix_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("custom") => {
            awaken_scenario_host::build_custom_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("delegate") => {
            awaken_scenario_host::build_delegation_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("statemachine") => {
            awaken_scenario_host::build_statemachine_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("statemachine-rich") => {
            awaken_scenario_host::build_statemachine_rich_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("config") => {
            awaken_scenario_host::build_config_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        // The management scenario drives the REAL management router but with the
        // deterministic MCP scenario model as an explicit HostExecutor selection;
        // env-driven store selection + IAM are identical to production.
        Ok(mode @ ("management" | "management-rate-limit")) => {
            let (model, model_ref) = awaken_scenario_host::scenario_model(
                std::sync::Arc::new(awaken_scenario_host::McpToolModel),
                "management",
            );
            let prepared = awaken_process_startup::prepare_all_in_one_process_with_scenario_model(
                model,
                model_ref,
                management_scenario_adapters(mode == "management-rate-limit"),
                Box::new(awaken_cli::LocalExecution::default()),
                &observability,
            )
            .await;
            scenario_lifecycle = Some(prepared.service_lifecycle);
            prepared.public_router
        }
        Ok("management-agents") => {
            let prepared = awaken_process_startup::prepare_all_in_one_process_with_scenario_model(
                std::sync::Arc::new(awaken_scenario_host::RegistryDelegatingModel),
                "management-agents".to_string(),
                awaken_process_startup::ManagedServiceAdapters::default(),
                Box::new(awaken_cli::LocalExecution::default()),
                &observability,
            )
            .await;
            scenario_lifecycle = Some(prepared.service_lifecycle);
            prepared.public_router
        }
        // Generic durable-operation fixtures need the production Config plane to
        // publish explicit Agent policy while a deterministic probe model drives
        // the tool call. This reuses AllInOne; it owns no parallel Host/store.
        Ok("management-probe") => {
            let prepared = awaken_process_startup::prepare_all_in_one_process_with_scenario_model(
                std::sync::Arc::new(awaken_scenario_host::ProbeModel),
                "management-probe".to_string(),
                awaken_process_startup::ManagedServiceAdapters::default(),
                Box::new(awaken_cli::LocalExecution::default()),
                &observability,
            )
            .await;
            scenario_lifecycle = Some(prepared.service_lifecycle);
            prepared.public_router
        }
        // Deterministic paid Web provider selected before AllInOne assembly, so
        // Control publication and Host dispatch consume one registry instance.
        Ok("management-web") => {
            let (router, lifecycle) =
                awaken_scenario_host::build_management_web_service(trace_file_present).await;
            scenario_lifecycle = Some(lifecycle);
            router
        }
        // Production model supply for provider/BYOK/brokered e2e: no
        // deterministic Host executor is installed, so publications resolve from
        // the authored catalog and execute through the real materializer.
        Ok("management-providers") => {
            let prepared = awaken_process_startup::prepare_all_in_one_process_with_execution(
                Box::new(awaken_cli::LocalExecution::default()),
                &observability,
            )
            .await;
            scenario_lifecycle = Some(prepared.service_lifecycle);
            prepared.public_router
        }
        Ok("distributed-provider") => awaken_scenario_host::build_distributed_provider_router(),
        // The production management deployment (durable stores + config plane +
        // resource PEP) with only its deterministic fallback model replaced. Used
        // by the Skill pin/restart e2e; resource repositories remain auth-agnostic.
        Ok("management-skills") => {
            let prepared = awaken_process_startup::prepare_all_in_one_process_with_scenario_model(
                std::sync::Arc::new(awaken_scenario_host::SkillDrivingModel),
                "management-skills".to_string(),
                awaken_process_startup::ManagedServiceAdapters::default(),
                Box::new(awaken_cli::LocalExecution::default()),
                &observability,
            )
            .await;
            scenario_lifecycle = Some(prepared.service_lifecycle);
            prepared.public_router
        }
        Ok("real") => {
            awaken_scenario_host::build_real_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("real-gemini") => {
            awaken_scenario_host::build_real_gemini_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("real-resolved") => {
            awaken_scenario_host::build_resolved_real_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("oauth-resolved") => {
            awaken_scenario_host::build_oauth_resolved_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("schedule") => {
            awaken_scenario_host::build_schedule_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("skills") => {
            awaken_scenario_host::build_skills_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("skills-durable") => {
            awaken_scenario_host::build_skills_durable_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("delegate-remote") => {
            awaken_scenario_host::build_remote_delegation_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("vision") => {
            awaken_scenario_host::build_vision_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("model-route") => {
            awaken_scenario_host::build_model_route_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        // Compatibility name only: both `acp` and `acp-sandboxed` resolve to
        // the one SessionEnvironment-owned ACP fixture. Keeping a direct Host
        // subprocess here would reopen the placement authority ADR-0073 removed.
        Ok("acp") => {
            awaken_scenario_host::build_acp_sandboxed_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-jsonrpc") => {
            awaken_scenario_host::build_acp_jsonrpc_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-permission") => {
            awaken_scenario_host::build_acp_permission_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-tool-bridge") => {
            awaken_scenario_host::build_acp_tool_bridge_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-control") => {
            awaken_scenario_host::build_acp_control_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-relaunch-failure") => {
            awaken_scenario_host::build_acp_relaunch_failure_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-sandboxed") => {
            awaken_scenario_host::build_acp_sandboxed_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-container") => {
            awaken_scenario_host::build_acp_container_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-gateway") => {
            awaken_scenario_host::build_acp_gateway_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("acp-managed-mcp") => {
            let (router, lifecycle) =
                awaken_scenario_host::build_acp_managed_mcp_service(trace_file_present).await;
            scenario_lifecycle = Some(lifecycle);
            router
        }
        Ok("acp-real-mcp") => {
            let (router, lifecycle) =
                awaken_scenario_host::build_acp_real_mcp_service(trace_file_present).await;
            scenario_lifecycle = Some(lifecycle);
            router
        }
        Ok("memory") => {
            awaken_scenario_host::build_memory_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("memory-resource") => {
            awaken_scenario_host::build_memory_resource_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("dream") => {
            awaken_scenario_host::build_dream_runtime_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("resource-scope-boundary") => {
            awaken_scenario_host::build_unscoped_resource_router(trace_file_present).await
        }
        Ok("resource-ephemeral") => {
            awaken_scenario_host::build_ephemeral_resource_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("git-repo") => {
            awaken_scenario_host::build_git_repo_router(
                awaken_scenario_host::scenario_deployment(),
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("compaction") => {
            awaken_scenario_host::build_compaction_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("error") => {
            awaken_scenario_host::build_error_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("worker") => {
            awaken_scenario_host::build_worker_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("environment-matrix") => {
            awaken_scenario_host::build_environment_matrix_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        Ok("full-chain") => {
            awaken_scenario_host::build_full_chain_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
        _ => {
            awaken_scenario_host::build_echo_router(
                trace_file_present,
                scenario_lifecycle
                    .as_ref()
                    .expect("Host-backed scenario retains its selected lifecycle")
                    .clone(),
            )
            .await
        }
    };
    // Brain admin surface (ADR-0022 D7): the connection-count metric that KEDA
    // autoscales on, plus /admin/drain + /readyz for graceful, stream-preserving
    // scale-in. Wraps the served router so the in-flight counter sees every request.
    let ctrl = awaken_process_startup::DrainController::new();
    // Register the connection-load gauge on the global OTel meter so `/metrics`
    // exposes `awaken_brain_active_streams` (the KEDA autoscale signal), matching the
    // production `awaken` binary. Held for the process lifetime so the callback stays
    // live — without this the drain/metrics e2e see the gauge missing.
    let _active_streams_gauge = awaken_process_startup::register_active_streams_gauge(ctrl.clone());
    let app = awaken_process_startup::with_process_admin(app, ctrl);
    // Root every request span in the ingress middleware (extracts the inbound
    // `traceparent`); the whole direct request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    // Enforce the managed-agents beta opt-in across ordinary Managed endpoint
    // families (bare router-level tests intentionally carry no such layer).
    let app = app.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let served = serve_scenario(app, &addr, scenario_lifecycle).await;
    // The original selected exporter closes only after the registered tasks join,
    // including bind/serve failures; preserve the original error after flushing.
    awaken_observability::shutdown();
    served
}

async fn serve_scenario(
    app: axum::Router,
    addr: &str,
    scenario_lifecycle: Option<awaken_service_lifecycle::ServiceLifecycle>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // A listener/serve failure is still a shutdown trigger after assembly has
    // registered background work. Preserve that result, drain the same owner,
    // and only then propagate it; an early `?` would skip the join.
    let served = async {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        eprintln!("awaken-coordinator listening on http://{addr}");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    }
    .await;
    // Shutdown decision table: C1 no Runtime work => immediate join; C2 a durable
    // dispatch or MemoryStore extraction remains active => the registered Host
    // drain keeps waiting; C3 work completes => all child and aux.background spans
    // end before exporter shutdown; C4 work never completes => the outer lifecycle
    // deadline reports the named task. The one
    // ServiceLifecycle mounted with the Router owns every rule; Router drop and
    // a timing sleep are not completion authorities.
    let shutdown = if let Some(lifecycle) = scenario_lifecycle {
        lifecycle.shutdown(std::time::Duration::from_secs(10)).await
    } else {
        Ok(())
    };
    // Preserve a simultaneous drain failure even when the original serving
    // error takes precedence. ShutdownError already names the timed-out tasks.
    if served.is_err()
        && let Err(error) = &shutdown
    {
        eprintln!("awaken-scenario-host: lifecycle shutdown after service failure: {error}");
    }
    served?;
    shutdown?;
    Ok(())
}

fn mode_uses_shared_scenario_runtime(mode: Option<&str>) -> bool {
    !matches!(
        mode,
        Some(
            "management"
                | "management-rate-limit"
                | "management-agents"
                | "management-probe"
                | "management-web"
                | "management-providers"
                | "management-skills"
                | "acp-managed-mcp"
                | "acp-real-mcp"
                | "distributed-control"
                | "distributed-provider"
        )
    )
}

#[expect(
    clippy::disallowed_methods,
    reason = "the scenario harness is its own process root and reads its scenario configuration from the environment"
)]
fn scenario_observability() -> awaken_observability::ObservabilityConfig {
    let mut config = awaken_observability::ObservabilityConfig {
        trace_file: std::env::var_os("AWAKEN_TRACE_FILE").map(Into::into),
        ..Default::default()
    };
    config.otel.endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    config.otel.traces_endpoint = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok();
    config.otel.protocol = std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    config.otel.traces_protocol = std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .ok()
        .and_then(|value| value.parse().ok());
    config.otel.service_name = std::env::var("OTEL_SERVICE_NAME").ok();
    config.otel.service_version = std::env::var("OTEL_SERVICE_VERSION").ok();
    config.otel.metric_export_interval = std::env::var("OTEL_METRIC_EXPORT_INTERVAL")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(config.otel.metric_export_interval);
    config
}

/// Resolve when the process is asked to stop, so `axum::serve` stops accepting new
/// connections and drains in-flight requests before returning. We wait on BOTH
/// SIGINT (Ctrl-C, a developer's foreground stop) AND SIGTERM (the signal an
/// orchestrator — Kubernetes, systemd, `docker stop` — sends first, before the
/// hard SIGKILL). Handling SIGTERM is what makes the drain fire under a real
/// deployment: a durable foreground run is held open by its still-pending HTTP
/// handler (which awaits the dispatch pool's settle event), so graceful shutdown
/// waits for that request to finish — the run settles and commits instead of being
/// abandoned mid-drive (stranded claimed-but-not-settled until its lease expires).
/// Without the SIGTERM arm the process took the default disposition — an immediate
/// kill (exit 143) that dropped in-flight foreground work and skipped the span
/// flush below. Background/queued pool work not tied to a live request is not
/// covered here; it relies on lease-expiry recovery on the next start.
#[expect(
    clippy::disallowed_methods,
    reason = "the scenario harness is its own process root and reads its scenario configuration from the environment"
)]
async fn shutdown_signal() {
    if std::env::var("AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF").as_deref() == Ok("1") {
        use tokio::io::{AsyncReadExt, stdin};

        let mut byte = [0_u8; 1];
        let _ = stdin().read(&mut byte).await;
        return;
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // If SIGTERM can't be registered, fall back to Ctrl-C only rather than
        // refuse to shut down.
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod dispatch_tests {
    //! Pin `main()`'s `AWAKEN_MODEL_MODE` dispatch: every mode string must route to a
    //! factory that actually assembles a mounted scenario router. The dispatch is an
    //! inline `match` in `main()` (not a `pub` fn we can call), so — refactor-free — this
    //! table MIRRORS those arms: each entry names the mode string and builds the SAME
    //! factory the arm calls, then asserts the router it produced exposes the shared
    //! protocol surface (`GET /v1/ai-sdk/threads/{id}/messages` → 200). A reviewer who
    //! edits a `match` arm must keep this table in step; a mode wired to a factory that
    //! fails to mount is caught here instead of only as a `e2e/*.mjs` boot failure.
    //!
    //! Covered here: every SYNC, environment-free arm. Excluded (documented, not unit-
    //! testable refactor-free): the arms that PANIC without operator env or external
    //! daemons — `real`/`real-gemini`/`real-resolved`/`oauth-resolved` (require a live
    //! API key / project), `delegate-remote` (`AWAKEN_REMOTE_AGENT_URL`), `acp-container`
    //! (a running Docker daemon), and the async management arms
    //! (`management`/`acp-managed-mcp`/`acp-real-mcp`). The two async but env-free arms
    //! (`config`, plus the default fallback) are asserted directly below.

    use awaken_scenario_host as sh;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The route every `mount`-based factory exposes — probe it to prove the arm's
    /// router mounted the shared protocol surface.
    const SHARED_PROBE: &str = "/v1/ai-sdk/threads/dispatch-probe/messages";

    async fn mounts_shared_surface(app: axum::Router) -> bool {
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri(SHARED_PROBE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
            == StatusCode::OK
    }

    #[tokio::test]
    async fn failed_listener_binding_joins_the_selected_lifecycle_before_returning() {
        // C1 startup already registered a service task; C2 this test holds the
        // exact listener address (no port-selection race); C3 bind fails. R1:
        // the actual root serve path must cancel AND join that same task before
        // returning the original AddrInUse. Success/EOF uses the same drain;
        // a later OS serve error shares this Result path but is not fault-injected
        // here. No Router-drop, retry, sleep or second lifecycle proves completion.
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap().to_string();
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (finished, mut completion) = tokio::sync::oneshot::channel();
        lifecycle.spawn("bind-failure-owned-task", move |cancel| async move {
            entered.send(()).unwrap();
            cancel.cancelled().await;
            finished.send(()).unwrap();
            Ok(())
        });
        entering.await.unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::serve_scenario(axum::Router::new(), &addr, Some(lifecycle)),
        )
        .await
        .expect("R1 the original failure path joins the selected task")
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::AddrInUse,
            "R1 preserve the original bind failure"
        );
        assert_eq!(
            completion.try_recv(),
            Ok(()),
            "R1 task completed before return"
        );
    }

    #[test]
    fn only_host_backed_modes_install_the_shared_scenario_runtime() {
        // Cause/effect decision table:
        // C1 a Host-backed scenario consumes deployment::resource_host -> E1 install
        // the single shared scenario authority (R1 default/ordinary mode).
        // C2 an AllInOne process owns its production authority -> E2 do not
        // install an unused parallel authority (R2 every management mode).
        // C3 split Control or protocol-only Provider owns no scenario Host -> E2
        // (R3 both distributed modes). These rows fence authority duplication while
        // preserving the durability guard for every mode that actually consumes it.
        assert!(super::mode_uses_shared_scenario_runtime(None), "R1");
        assert!(
            super::mode_uses_shared_scenario_runtime(Some("probe")),
            "R1"
        );
        for mode in [
            "management",
            "management-rate-limit",
            "management-agents",
            "management-probe",
            "management-web",
            "management-providers",
            "management-skills",
            "acp-managed-mcp",
            "acp-real-mcp",
        ] {
            assert!(
                !super::mode_uses_shared_scenario_runtime(Some(mode)),
                "R2 {mode}"
            );
        }
        for mode in ["distributed-control", "distributed-provider"] {
            assert!(
                !super::mode_uses_shared_scenario_runtime(Some(mode)),
                "R3 {mode}"
            );
        }
    }

    #[tokio::test]
    async fn every_sync_env_free_mode_string_maps_to_a_mounted_factory() {
        // Cause/effect decision table: each supported environment-free mode is
        // one cause; every rule must build through its canonical scenario
        // platform and expose the shared protocol route. A missing Resources
        // application, alternate Host path, or unmounted adapter is a failed
        // effect for that exact mode. Modes that instantiate an injectable
        // sandbox receive the same explicit Local test deployment, so this
        // router-mapping test does not depend on an ambient Namespace companion.
        // `acp-tool-bridge` deliberately consumes the real scenario deployment
        // and sibling Hand binary; its dedicated durable ACP E2E owns that row.
        // C0 all mode factories in one async table => their large construction
        // futures compound this test frame. Pin each future on the heap before
        // awaiting; the default-stack regression must execute every original
        // mode/argument and retain the same mounted-route assertion below.
        // Stack decision rows: L5 unchanged 25 factories/order/Local deployment
        // and this original current-thread test, with the full authored feature
        // graph and no RUST_MIN_STACK override -> one real test, all routes200.
        // L6 removing only these 25 heap wrappers -> observe this exact frame's
        // original stack-overflow outcome; if it does not recur, report that
        // result rather than add a factory, shrink a stack or change a selector.
        // Service/composed-test32MiB wrappers and the Local tier's recursive Box
        // exercise different boundaries. This table calls factories directly;
        // main's inline mode selection still needs its own static/e2e evidence.
        // (AWAKEN_MODEL_MODE string, the router its `main()` arm builds). Kept in the
        // same order as the `match` in `main()` so drift is easy to spot.
        // O1/O6: all25 pure Router fixtures retain independent lifecycles; each
        // original route200 assertion precedes joining that fixture's actual
        // registered work. Identical task names never select a global owner.
        let mut local_test_deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        local_test_deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
        let dispatch: Vec<(
            &str,
            axum::Router,
            awaken_service_lifecycle::ServiceLifecycle,
        )> = vec![
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "probe",
                    Box::pin(sh::build_probe_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "revise",
                    Box::pin(sh::build_router(
                        std::sync::Arc::new(sh::ReviseModel),
                        "revise",
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "custom",
                    Box::pin(sh::build_custom_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "delegate",
                    Box::pin(sh::build_delegation_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "statemachine",
                    Box::pin(sh::build_statemachine_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "statemachine-rich",
                    Box::pin(sh::build_statemachine_rich_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "schedule",
                    Box::pin(sh::build_schedule_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "skills",
                    Box::pin(sh::build_skills_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "skills-durable",
                    Box::pin(sh::build_skills_durable_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "vision",
                    Box::pin(sh::build_vision_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "model-route",
                    Box::pin(sh::build_model_route_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "acp",
                    Box::pin(sh::build_acp_sandboxed_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "acp-jsonrpc",
                    Box::pin(sh::build_acp_jsonrpc_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "acp-permission",
                    Box::pin(sh::build_acp_permission_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "acp-sandboxed",
                    Box::pin(sh::build_acp_sandboxed_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "acp-gateway",
                    Box::pin(sh::build_acp_gateway_router_with_deployment(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "memory",
                    Box::pin(sh::build_memory_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "memory-resource",
                    Box::pin(sh::build_memory_resource_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "git-repo",
                    Box::pin(sh::build_git_repo_router(
                        local_test_deployment.clone(),
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "compaction",
                    Box::pin(sh::build_compaction_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "error",
                    Box::pin(sh::build_error_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "worker",
                    Box::pin(sh::build_worker_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "environment-matrix",
                    Box::pin(sh::build_environment_matrix_router(
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "full-chain",
                    Box::pin(sh::build_full_chain_router_with_deployment(
                        local_test_deployment,
                        false,
                        lifecycle.clone(),
                    ))
                    .await,
                    lifecycle,
                )
            },
            // The `_ =>` fallback: any unrecognized mode routes to the echo router.
            {
                let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
                (
                    "<default/unknown>",
                    Box::pin(sh::build_echo_router(false, lifecycle.clone())).await,
                    lifecycle,
                )
            },
        ];
        for (mode, app, lifecycle) in dispatch {
            assert!(
                mounts_shared_surface(app).await,
                "AWAKEN_MODEL_MODE={mode} must map to a factory that mounts the shared surface"
            );
            lifecycle
                .shutdown(std::time::Duration::from_secs(10))
                .await
                .expect("O1 each independent route fixture drains its own owner");
        }
    }

    #[tokio::test]
    async fn resource_scope_boundary_mode_intentionally_mounts_only_raw_resource_routes() {
        let response = sh::build_unscoped_resource_router(false)
            .await
            .oneshot(
                Request::builder()
                    .uri("/v1/files")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // Multi-thread matches the production host while admin-assistant seeding resolves
    // through the scenario's async model-publication adapter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_config_mode_maps_to_the_config_factory() {
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();

        // `config` is async but environment-free (in-memory SQLite); assert it too.
        assert!(
            mounts_shared_surface(sh::build_config_router(false, lifecycle.clone()).await).await,
            "AWAKEN_MODEL_MODE=config must map to a mounted config factory"
        );

        lifecycle
            .shutdown(std::time::Duration::from_secs(10))
            .await
            .expect("O1 config fixture drains its selected owner");
    }
}
