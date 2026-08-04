//! `awaken` — explicit process roles over one shared composition.

mod console;

use std::process::ExitCode;

use awaken_cli::config::{ConfigOverrides, ResolvedDeployment, Role};

#[derive(Clone, Copy)]
enum Presentation {
    Interactive,
    Headless,
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match console::parse_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("awaken: {error}\n");
            console::print_help();
            return ExitCode::FAILURE;
        }
    };

    match run(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: console::Command) -> Result<(), String> {
    match command {
        console::Command::Help => {
            console::print_help();
            Ok(())
        }
        console::Command::Version => {
            println!("awaken {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        console::Command::Config { json, config_path } => {
            let deployment = ResolvedDeployment::load(ConfigOverrides {
                config_path,
                ..Default::default()
            })?;
            println!("{}", deployment.report(json).trim_end());
            Ok(())
        }
        console::Command::DoctorAcp { json } => {
            println!(
                "{}",
                awaken_cli::local_acp_diagnostics(json).await.trim_end()
            );
            Ok(())
        }
        console::Command::DatabaseMigrate { config_path } => {
            let deployment = load_migration_deployment(config_path)?;
            warn_deprecations(&deployment);
            deployment.ensure_data_layout()?;
            let seal_key = role_seal_key(&deployment)?;
            awaken_cli::migrate_deployment_schema(&deployment, seal_key.as_ref()).await
        }
        console::Command::ControlIamProfile => {
            println!(
                "{}",
                serde_json::to_string_pretty(&awaken_control::management_authorization_profile())
                    .map_err(|error| format!("serialize Control IAM profile: {error}"))?
            );
            Ok(())
        }
        console::Command::ControlIamResourceProfile => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &awaken_control::management_resource_authorization_profile()
                )
                .map_err(|error| format!("serialize Control resource IAM profile: {error}"))?
            );
            Ok(())
        }
        console::Command::ControlIamRuntimeProfile => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &awaken_control::hosted_runtime_authorization_profile()
                )
                .map_err(|error| format!("serialize Hosted Runtime IAM profile: {error}"))?
            );
            Ok(())
        }
        console::Command::Worker {
            server,
            config_path,
        } => {
            let deployment = ResolvedDeployment::load(ConfigOverrides {
                config_path,
                role: Some(Role::Worker),
                worker_server: Some(server.clone()),
                ..Default::default()
            })?;
            warn_deprecations(&deployment);
            deployment.ensure_data_layout()?;
            awaken_observability::init(&deployment.observability);
            let result = match awaken_cli::build_configured_worker(&server, &deployment).await {
                Ok(worker) => worker
                    .run_until_shutdown()
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error),
            };
            awaken_observability::shutdown();
            result
        }
        console::Command::AllInOne(args) => {
            run_service(args, Presentation::Interactive, Role::AllInOne).await
        }
        console::Command::Control(args) => {
            run_service(args, Presentation::Headless, Role::Control).await
        }
        console::Command::Coordinator(args) => {
            run_service(args, Presentation::Headless, Role::Coordinator).await
        }
    }
}

fn load_migration_deployment(
    config_path: Option<std::path::PathBuf>,
) -> Result<ResolvedDeployment, String> {
    // Migration selects storage adapters from the deployment's authored role.
    // Overriding it to AllInOne changes private-boundary validation and makes a
    // valid split Coordinator configuration impossible to migrate.
    ResolvedDeployment::load(ConfigOverrides {
        config_path,
        ..Default::default()
    })
}

fn role_seal_key(deployment: &ResolvedDeployment) -> Result<Option<[u8; 32]>, String> {
    match deployment.role {
        Role::AllInOne | Role::Control => deployment.seal_key.load_or_create().map(Some),
        Role::Coordinator | Role::Worker => Ok(None),
    }
}

async fn run_service(
    args: console::ServiceArgs,
    presentation: Presentation,
    role: Role,
) -> Result<(), String> {
    let mut deployment = ResolvedDeployment::load(ConfigOverrides {
        config_path: args.config_path,
        role: Some(role),
        data_dir: args.data_dir,
        port: args.port,
        no_browser: args.no_browser.then_some(true),
        identity_mode: args.identity_mode,
        cloud_models: args.cloud_models,
        ..Default::default()
    })?;
    if matches!(role, Role::Control | Role::Coordinator)
        && deployment.mode != awaken_cli::config::OperatingMode::Server
    {
        return Err(format!(
            "`awaken {}` requires mode = \"server\" in config.toml",
            role.as_str()
        ));
    }
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    let seal_key = role_seal_key(&deployment)?;
    let local_worker = if role == Role::AllInOne {
        Some(
            awaken_cli::prepare_local_worker(
                &mut deployment,
                seal_key.as_ref().expect("AllInOne owns Control seal key"),
            )
            .await?,
        )
    } else {
        None
    };
    if role != Role::Control
        && let Some(error) = deployment.runtime.durable_needs_persistence_error(false)
    {
        return Err(error.to_owned());
    }

    awaken_observability::init(&deployment.observability);
    let result = serve_resolved(deployment, seal_key, presentation, role, local_worker).await;
    awaken_observability::shutdown();
    result
}

async fn serve_resolved(
    deployment: ResolvedDeployment,
    seal_key: Option<[u8; 32]>,
    presentation: Presentation,
    role: Role,
    prepared_worker: Option<awaken_cli::PreparedLocalWorker>,
) -> Result<(), String> {
    let assembly = match role {
        Role::AllInOne => {
            awaken_cli::build_all_in_one_assembly(
                &deployment,
                seal_key.as_ref().expect("AllInOne owns Control seal key"),
            )
            .await?
        }
        Role::Control => {
            awaken_cli::build_control_assembly(
                &deployment,
                seal_key.as_ref().expect("Control owns Control seal key"),
            )
            .await?
        }
        Role::Coordinator => awaken_cli::build_coordinator_assembly(&deployment).await?,
        Role::Worker => unreachable!("Worker has its own process composition"),
    };
    let local_setup = assembly.local_setup;
    let registration_supervisor = assembly.registration_supervisor;
    // The beta gate is a composition-edge concern: it wraps both Session and
    // control-plane Managed families while leaving AI SDK/A2A and family-specific
    // betas untouched. Router-level domain tests intentionally remain headerless.
    let app = assembly.router.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let ctrl = awaken_cli::DrainController::new();
    if let Some(supervisor) = registration_supervisor {
        ctrl.set_registration_supervisor(supervisor);
    }
    let _active_streams_gauge = awaken_cli::register_active_streams_gauge(ctrl.clone());
    let app = match &deployment.admin_listen {
        Some(_) => awaken_cli::with_connection_metric(app, ctrl.clone()),
        None => awaken_cli::with_process_admin(app, ctrl.clone()),
    };
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    let app = awaken_cli::mount_console_with_navigation(
        app,
        awaken_api_contract::SuiteNavigation {
            hub_url: deployment.suite_hub_url.clone(),
        },
    );

    if let Some(admin_addr) = &deployment.admin_listen {
        let listener = tokio::net::TcpListener::bind(admin_addr)
            .await
            .map_err(|error| friendly_bind_error("admin", admin_addr, error))?;
        let admin = awaken_cli::process_admin_router(ctrl);
        eprintln!("awaken: admin http://{admin_addr} (/readyz /metrics /admin/drain)");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, admin).await {
                eprintln!("awaken: admin server stopped: {error}");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&deployment.bind)
        .await
        .map_err(|error| friendly_bind_error("server", &deployment.bind, error))?;
    let url = browser_url(&deployment.bind)?;
    let local_worker = prepared_worker
        .map(|prepared| prepared.build_worker(url.clone(), &deployment))
        .transpose()?;
    match presentation {
        Presentation::Interactive => {
            eprintln!("\n  Awaken is ready\n");
            eprintln!("  Console   {url}");
            if let Some(setup) = &local_setup {
                eprintln!("  Setup     {}", setup.setup_token);
                eprintln!("  Expires   {}", setup.expires_at.0);
            }
            eprintln!("  Data      {}", deployment.data_dir.display());
            eprintln!("  Stop      Ctrl+C\n");
            if !deployment.no_browser
                && let Err(error) = open_browser(&url)
            {
                eprintln!("awaken: could not open a browser ({error}); open {url} manually");
            }
        }
        Presentation::Headless => {
            eprintln!(
                "awaken: ready url={url} data_dir={} mode={}",
                deployment.data_dir.display(),
                deployment.mode.as_str()
            );
            if let Some(setup) = &local_setup {
                eprintln!(
                    "awaken: local setup token={} expires_at={}",
                    setup.setup_token, setup.expires_at.0
                );
            }
        }
    }

    let Some(worker) = local_worker else {
        return axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|error| format!("server stopped: {error}"));
    };

    // AllInOne owns one shutdown sequence. Keep the Control HTTP endpoint alive
    // until its co-located Worker has fenced admission and deregistered; only
    // then stop accepting external traffic. Independent signal listeners race
    // and can otherwise abort the Worker after the server has already made its
    // required drain/deregister calls unreachable.
    let (worker_shutdown_tx, worker_shutdown_rx) = tokio::sync::oneshot::channel();
    let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = server_shutdown_rx.await;
        }),
    );
    tokio::pin!(server);
    let worker = tokio::spawn(async move {
        worker
            .run_until(async move {
                worker_shutdown_rx.await.map_err(|_| {
                    Box::new(std::io::Error::other(
                        "AllInOne Worker shutdown owner dropped",
                    )) as Box<dyn std::error::Error + Send + Sync>
                })
            })
            .await
            .map_err(|error| error.to_string())
    });
    tokio::pin!(worker);
    let mut worker_shutdown_tx = Some(worker_shutdown_tx);
    let mut server_shutdown_tx = Some(server_shutdown_tx);
    tokio::select! {
        mode = shutdown_mode_signal() => {
            if let Some(tx) = worker_shutdown_tx.take() {
                let _ = tx.send(mode);
            }
            let worker_result = local_worker_result((&mut worker).await);
            if let Some(tx) = server_shutdown_tx.take() {
                let _ = tx.send(());
            }
            let server_result = (&mut server)
                .await
                .map_err(|error| format!("server stopped: {error}"));
            worker_result?;
            server_result
        }
        result = &mut server => {
            if let Some(tx) = worker_shutdown_tx.take() {
                let _ = tx.send(awaken_worker::WorkerShutdown::Prompt);
            }
            let worker_result = local_worker_result((&mut worker).await);
            result.map_err(|error| format!("server stopped: {error}"))?;
            worker_result
        }
        result = &mut worker => {
            let worker_result = local_worker_result(result);
            if let Some(tx) = server_shutdown_tx.take() {
                let _ = tx.send(());
            }
            let server_result = (&mut server)
                .await
                .map_err(|error| format!("server stopped: {error}"));
            worker_result?;
            server_result
        }
    }
}

fn local_worker_result(
    result: Result<Result<(), String>, tokio::task::JoinError>,
) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("local Worker stopped: {error}")),
        Err(error) => Err(format!("local Worker task failed: {error}")),
    }
}

fn warn_deprecations(deployment: &ResolvedDeployment) {
    for warning in &deployment.deprecations {
        eprintln!("awaken: warning: {warning}");
    }
}

fn friendly_bind_error(kind: &str, address: &str, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        format!(
            "cannot start {kind} on {address}: the address is already in use; choose another with --port or bind in the deployment config"
        )
    } else {
        format!("cannot bind {kind} to {address}: {error}")
    }
}

fn browser_url(bind: &str) -> Result<String, String> {
    let address = bind
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("invalid bind address {bind:?}"))?;
    let host = if address.ip().is_unspecified() {
        if address.is_ipv6() {
            "[::1]"
        } else {
            "127.0.0.1"
        }
    } else if address.is_ipv6() {
        return Ok(format!("http://[{}]:{}", address.ip(), address.port()));
    } else {
        return Ok(format!("http://{}:{}", address.ip(), address.port()));
    };
    Ok(format!("http://{host}:{}", address.port()))
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("no desktop session detected".to_owned());
        }
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Resolve the one process shutdown reason. AllInOne passes this same decision
/// into its Worker before stopping the local Control/Coordinator HTTP endpoint.
async fn shutdown_mode_signal() -> awaken_worker::WorkerShutdown {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return awaken_worker::WorkerShutdown::Prompt;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => awaken_worker::WorkerShutdown::Prompt,
            _ = term.recv() => awaken_worker::WorkerShutdown::Graceful,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        awaken_worker::WorkerShutdown::Prompt
    }
}

async fn shutdown_signal() {
    let _ = shutdown_mode_signal().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_the_typed_service_role() {
        // Cause/effect decision table: M1 Coordinator config with token-only
        // server credentials -> resolves as Coordinator; M2 config without a
        // role -> retains the canonical AllInOne default. Neither case invents
        // a migration-only role or a second configuration path.
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token\n").unwrap();
        let coordinator = dir.path().join("coordinator.toml");
        std::fs::write(
            &coordinator,
            format!(
                r#"
data_dir = {data:?}
mode = "server"
role = "coordinator"
runtime_database_url = "postgres://127.0.0.1/runtime"
resource_database_url = "postgres://127.0.0.1/resources"
executable_agent_registration_token_file = {token:?}
control_internal_url = "http://127.0.0.1:3000"
control_service_token_file = {token:?}
"#,
                data = dir.path().join("coordinator-data"),
                token = token,
            ),
        )
        .unwrap();
        assert_eq!(
            load_migration_deployment(Some(coordinator)).unwrap().role,
            Role::Coordinator,
            "M1"
        );

        let all_in_one = dir.path().join("all-in-one.toml");
        std::fs::write(
            &all_in_one,
            format!(
                "data_dir = {:?}\nidentity_mode = \"no-login\"\n",
                dir.path().join("all-in-one-data")
            ),
        )
        .unwrap();
        assert_eq!(
            load_migration_deployment(Some(all_in_one)).unwrap().role,
            Role::AllInOne,
            "M2"
        );
    }

    #[test]
    fn unspecified_bind_opens_the_loopback_console() {
        assert_eq!(
            browser_url("0.0.0.0:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(browser_url("[::]:8080").unwrap(), "http://[::1]:8080");
    }

    #[test]
    fn address_conflict_has_an_actionable_error() {
        let message = friendly_bind_error(
            "server",
            "127.0.0.1:8080",
            std::io::Error::from(std::io::ErrorKind::AddrInUse),
        );
        assert!(message.contains("already in use"));
        assert!(message.contains("--port"));
    }
}
