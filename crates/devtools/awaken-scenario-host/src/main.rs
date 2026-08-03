//! `awaken-coordinator` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised; `=vision` reports the media it
//! received so the multimodal path can be verified. A provider executor is
//! swapped in through [`awaken_scenario_host::build_router`].

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Scenario environment is test-harness input, translated here into the same
    // typed policy production receives from config.toml. The observability adapter
    // itself has no ambient compatibility path.
    let observability = scenario_observability();
    awaken_observability::init(&observability);
    // The scenario-only role axis (`AWAKEN_SCENARIO_ROLE`). Worker is an execution
    // endpoint that never starts the HTTP surface; Serve is the default —
    // single-machine all-in-one, or a coordinator when the local pool is disabled.
    // The hand is the separate `awaken-sandbox hand` execution-plane binary.
    if std::env::var("AWAKEN_SCENARIO_ROLE").as_deref() == Ok("worker") {
        let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
        let worker_id = std::env::var("AWAKEN_WORKER_ID")
            .unwrap_or_else(|_| "awaken-scenario-worker".to_string());
        let admin_listen = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")
            .ok()
            .filter(|address| !address.trim().is_empty());
        let request_authorizer = std::env::var("AWAKEN_WORKER_REQUEST_CREDENTIAL_FILE")
            .ok()
            .map(|path| {
                awaken_cli::load_worker_request_authorizer(std::path::Path::new(&path), &worker_id)
            })
            .transpose()?;
        // Test-only echo-draining worker (the worker-pool e2e). The production
        // worker with real per-run model resolution lives in `awaken-worker`.
        return awaken_scenario_host::run_echo_worker(
            &upstream,
            &worker_id,
            admin_listen.as_deref(),
            request_authorizer,
        )
        .await;
    }
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    // Durability guard: refuse to boot a `typed durable ingress` ingress that would
    // resolve to a volatile in-memory queue (durable + default sqlite backend + no
    // DeploymentConfig::storage_dir) — such a queue silently drops every queued/crashed/scheduled
    // run on restart, defeating the whole point of durable ingress (no-data-loss).
    let deployment = awaken_scenario_host::scenario_deployment();
    if let Some(error) = deployment.durable_needs_persistence_error(false) {
        return Err(error.to_owned().into());
    }
    // The scenario host is a Local composition and therefore reuses the same
    // canonical Coordinator migrate-and-connect path as Local AllInOne.
    awaken_coordinator::open_coordinator_persistence(&deployment)
        .await
        .map(drop)?;
    let app = match std::env::var("AWAKEN_MODEL_MODE").as_deref() {
        Ok("probe") => {
            awaken_scenario_host::build_router(Arc::new(awaken_scenario_host::ProbeModel), "probe")
        }
        Ok("revise") => awaken_scenario_host::build_router(
            Arc::new(awaken_scenario_host::ReviseModel),
            "revise",
        ),
        Ok("outcome-matrix") => awaken_scenario_host::build_outcome_matrix_router(),
        Ok("custom") => awaken_scenario_host::build_custom_router(),
        Ok("delegate") => awaken_scenario_host::build_delegation_router(),
        Ok("statemachine") => awaken_scenario_host::build_statemachine_router(),
        Ok("statemachine-rich") => awaken_scenario_host::build_statemachine_rich_router(),
        Ok("config") => awaken_scenario_host::build_config_router().await,
        // The management scenario drives the REAL management router but with the
        // deterministic MCP scenario model as an explicit HostExecutor composition;
        // env-driven store selection + IAM are identical to production.
        Ok("management") => {
            let (model, model_ref) = awaken_scenario_host::scenario_model(
                std::sync::Arc::new(awaken_scenario_host::McpToolModel),
                "management",
            );
            awaken_cli::build_all_in_one_router_with_scenario_model(model, model_ref).await
        }
        Ok("management-agents") => {
            awaken_cli::build_all_in_one_router_with_scenario_model(
                std::sync::Arc::new(awaken_scenario_host::RegistryDelegatingModel),
                "management-agents".to_string(),
            )
            .await
        }
        // Production model composition for provider/BYOK/brokered e2e: no
        // deterministic Host executor is installed, so publications resolve from
        // the authored catalog and execute through the real materializer.
        Ok("management-providers") => awaken_cli::build_all_in_one_router().await,
        // Real split-Control composition with only its model-publication SPI
        // supplied by the deterministic scenario adapter.
        Ok("distributed-control") => awaken_scenario_host::build_distributed_control_router().await,
        Ok("distributed-provider") => awaken_scenario_host::build_distributed_provider_router(),
        // The production management composition (durable stores + config plane +
        // resource PEP) with only its deterministic fallback model replaced. Used
        // by the Skill pin/restart e2e; resource repositories remain auth-agnostic.
        Ok("management-skills") => {
            awaken_cli::build_all_in_one_router_with_scenario_model(
                std::sync::Arc::new(awaken_scenario_host::SkillDrivingModel),
                "management-skills".to_string(),
            )
            .await
        }
        Ok("real") => awaken_scenario_host::build_real_router(),
        Ok("real-gemini") => awaken_scenario_host::build_real_gemini_router().await,
        Ok("real-resolved") => awaken_scenario_host::build_resolved_real_router().await,
        Ok("oauth-resolved") => awaken_scenario_host::build_oauth_resolved_router().await,
        Ok("schedule") => awaken_scenario_host::build_schedule_router(),
        Ok("skills") => awaken_scenario_host::build_skills_router(),
        Ok("skills-durable") => awaken_scenario_host::build_skills_durable_router().await,
        Ok("delegate-remote") => awaken_scenario_host::build_remote_delegation_router(),
        Ok("vision") => awaken_scenario_host::build_vision_router(),
        Ok("model-route") => awaken_scenario_host::build_model_route_router(),
        Ok("acp") => awaken_scenario_host::build_acp_router(),
        Ok("acp-jsonrpc") => awaken_scenario_host::build_acp_jsonrpc_router().await,
        Ok("acp-permission") => awaken_scenario_host::build_acp_permission_router(),
        Ok("acp-control") => awaken_scenario_host::build_acp_control_router(),
        Ok("acp-relaunch-failure") => awaken_scenario_host::build_acp_relaunch_failure_router(),
        Ok("acp-sandboxed") => awaken_scenario_host::build_acp_sandboxed_router().await,
        Ok("acp-container") => awaken_scenario_host::build_acp_container_router().await,
        Ok("acp-gateway") => awaken_scenario_host::build_acp_gateway_router(),
        Ok("acp-managed-mcp") => awaken_scenario_host::build_acp_managed_mcp_router().await,
        Ok("acp-real-mcp") => awaken_scenario_host::build_acp_real_mcp_router().await,
        Ok("memory") => awaken_scenario_host::build_memory_router(),
        Ok("memory-resource") => awaken_scenario_host::build_memory_resource_router(),
        Ok("dream") => awaken_scenario_host::build_dream_router(),
        Ok("resource-scope-boundary") => awaken_scenario_host::build_unscoped_resource_router(),
        Ok("resource-ephemeral") => awaken_scenario_host::build_ephemeral_resource_router(),
        Ok("git-repo") => awaken_scenario_host::build_git_repo_router(),
        Ok("compaction") => awaken_scenario_host::build_compaction_router(),
        Ok("error") => awaken_scenario_host::build_error_router(),
        Ok("worker") => awaken_scenario_host::build_worker_router(),
        Ok("environment-matrix") => awaken_scenario_host::build_environment_matrix_router(),
        Ok("full-chain") => awaken_scenario_host::build_full_chain_router(),
        _ => awaken_scenario_host::build_echo_router(),
    };
    // Brain admin surface (ADR-0022 D7): the connection-count metric that KEDA
    // autoscales on, plus /admin/drain + /readyz for graceful, stream-preserving
    // scale-in. Wraps the served router so the in-flight counter sees every request.
    let ctrl = awaken_cli::DrainController::new();
    // Register the connection-load gauge on the global OTel meter so `/metrics`
    // exposes `awaken_brain_active_streams` (the KEDA autoscale signal), matching the
    // production `awaken` binary. Held for the process lifetime so the callback stays
    // live — without this the drain/metrics e2e see the gauge missing.
    let _active_streams_gauge = awaken_cli::register_active_streams_gauge(ctrl.clone());
    let app = awaken_cli::with_process_admin(app, ctrl);
    // Root every request span in the ingress middleware (extracts the inbound
    // `traceparent`); the whole direct request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    // Enforce the managed-agents beta opt-in across ordinary Managed endpoint
    // families (bare router-level tests intentionally carry no such layer).
    let app = app.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("awaken-coordinator listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Flush any buffered spans (OTLP batch / trace-file) before exit.
    awaken_observability::shutdown();
    Ok(())
}

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
    async fn every_sync_env_free_mode_string_maps_to_a_mounted_factory() {
        // Cause/effect decision table: each supported environment-free mode is
        // one cause; every rule must build through its canonical scenario
        // composition and expose the shared protocol route. A missing Resources
        // application, alternate Host path, or unmounted adapter is a failed
        // effect for that exact mode.
        // (AWAKEN_MODEL_MODE string, the router its `main()` arm builds). Kept in the
        // same order as the `match` in `main()` so drift is easy to spot.
        let mut local_test_deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
        local_test_deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
        let dispatch: Vec<(&str, axum::Router)> = vec![
            (
                "probe",
                sh::build_router(std::sync::Arc::new(sh::ProbeModel), "probe"),
            ),
            (
                "revise",
                sh::build_router(std::sync::Arc::new(sh::ReviseModel), "revise"),
            ),
            ("custom", sh::build_custom_router()),
            ("delegate", sh::build_delegation_router()),
            ("statemachine", sh::build_statemachine_router()),
            ("statemachine-rich", sh::build_statemachine_rich_router()),
            ("schedule", sh::build_schedule_router()),
            ("skills", sh::build_skills_router()),
            ("skills-durable", sh::build_skills_durable_router().await),
            ("vision", sh::build_vision_router()),
            ("model-route", sh::build_model_route_router()),
            ("acp", sh::build_acp_router()),
            ("acp-jsonrpc", sh::build_acp_jsonrpc_router().await),
            ("acp-permission", sh::build_acp_permission_router()),
            (
                "acp-sandboxed",
                sh::build_acp_sandboxed_router_with_deployment(local_test_deployment).await,
            ),
            ("acp-gateway", sh::build_acp_gateway_router()),
            ("memory", sh::build_memory_router()),
            ("memory-resource", sh::build_memory_resource_router()),
            ("git-repo", sh::build_git_repo_router()),
            ("compaction", sh::build_compaction_router()),
            ("error", sh::build_error_router()),
            ("worker", sh::build_worker_router()),
            ("environment-matrix", sh::build_environment_matrix_router()),
            ("full-chain", sh::build_full_chain_router()),
            // The `_ =>` fallback: any unrecognized mode routes to the echo router.
            ("<default/unknown>", sh::build_echo_router()),
        ];
        for (mode, app) in dispatch {
            assert!(
                mounts_shared_surface(app).await,
                "AWAKEN_MODEL_MODE={mode} must map to a factory that mounts the shared surface"
            );
        }
    }

    #[tokio::test]
    async fn resource_scope_boundary_mode_intentionally_mounts_only_raw_resource_routes() {
        let response = sh::build_unscoped_resource_router()
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
        // `config` is async but environment-free (in-memory SQLite); assert it too.
        assert!(
            mounts_shared_surface(sh::build_config_router().await).await,
            "AWAKEN_MODEL_MODE=config must map to a mounted config factory"
        );
    }
}
