//! Canonical process lifecycle shared by the product launcher and thin service binaries.

use std::process::ExitCode;

use crate::config::{ConfigOverrides, ResolvedDeployment, Role};
use crate::console::{ServiceArgs, ServiceBinaryCommand};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRole {
    AllInOne,
    Control,
    Coordinator,
}

impl ServiceRole {
    fn deployment_role(self) -> Role {
        match self {
            Self::AllInOne => Role::AllInOne,
            Self::Control => Role::Control,
            Self::Coordinator => Role::Coordinator,
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Self::AllInOne => "awaken",
            Self::Control => "awaken-control",
            Self::Coordinator => "awaken-coordinator",
        }
    }
}

/// Run one role-specific executable over the canonical service lifecycle.
pub async fn run_service_binary(role: ServiceRole) -> ExitCode {
    let binary = role.binary();
    let command = match crate::console::parse_service_binary_command(std::env::args().skip(1)) {
        Ok(ServiceBinaryCommand::Help) => {
            crate::console::print_service_help(binary);
            return ExitCode::SUCCESS;
        }
        Ok(command) => command,
        Err(error) => {
            eprintln!("{binary}: {error}\n");
            crate::console::print_service_help(binary);
            return ExitCode::FAILURE;
        }
    };
    let result = match command {
        ServiceBinaryCommand::Serve(args) => run_service(args, role).await,
        ServiceBinaryCommand::DatabaseMigrate { config_path } => {
            migrate_service_for_role(config_path, role).await
        }
        ServiceBinaryCommand::Help => unreachable!("help returned before service dispatch"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{binary}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Run the exact Control, Coordinator, or AllInOne composition selected by the
/// caller. Role-specific binaries and `awaken <role>` both terminate here.
pub async fn run_service(args: ServiceArgs, role: ServiceRole) -> Result<(), String> {
    let deployment_role = role.deployment_role();
    let mut deployment = ResolvedDeployment::load(ConfigOverrides {
        config_path: args.config_path,
        role: Some(deployment_role),
        data_dir: args.data_dir,
        port: args.port,
        no_browser: args.no_browser.then_some(true),
        identity_mode: args.identity_mode,
        cloud_models: args.cloud_models,
        ..Default::default()
    })?;
    if matches!(role, ServiceRole::Control | ServiceRole::Coordinator)
        && deployment.mode != crate::config::OperatingMode::Server
    {
        return Err(format!(
            "`{}` requires mode = \"server\" in config.toml",
            role.binary()
        ));
    }
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    let seal_key = role_seal_key(&deployment)?;
    let local_worker = if role == ServiceRole::AllInOne {
        Some(
            crate::prepare_local_worker(
                &mut deployment,
                seal_key.as_ref().expect("AllInOne owns Control seal key"),
            )
            .await?,
        )
    } else {
        None
    };
    if role != ServiceRole::Control
        && let Some(error) = deployment.runtime.durable_needs_persistence_error(false)
    {
        return Err(error.to_owned());
    }

    awaken_observability::init(&deployment.observability);
    let result = serve_resolved(deployment, seal_key, role, local_worker).await;
    awaken_observability::shutdown();
    result
}

/// Apply only the migrations owned by the role declared in the deployment.
pub async fn migrate_service(config_path: Option<std::path::PathBuf>) -> Result<(), String> {
    let deployment = load_migration_deployment(config_path)?;
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    let seal_key = role_seal_key(&deployment)?;
    crate::migrate_deployment_schema(&deployment, seal_key.as_ref()).await
}

async fn migrate_service_for_role(
    config_path: Option<std::path::PathBuf>,
    role: ServiceRole,
) -> Result<(), String> {
    let deployment = ResolvedDeployment::load(ConfigOverrides {
        config_path,
        role: Some(role.deployment_role()),
        ..Default::default()
    })?;
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    let seal_key = role_seal_key(&deployment)?;
    crate::migrate_deployment_schema(&deployment, seal_key.as_ref()).await
}

fn load_migration_deployment(
    config_path: Option<std::path::PathBuf>,
) -> Result<ResolvedDeployment, String> {
    // Migration selects storage adapters from the authored role. Overriding the
    // role would change its private-boundary validation and authority manifest.
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

async fn serve_resolved(
    deployment: ResolvedDeployment,
    seal_key: Option<[u8; 32]>,
    role: ServiceRole,
    prepared_worker: Option<crate::PreparedLocalWorker>,
) -> Result<(), String> {
    let assembly = match role {
        ServiceRole::AllInOne => {
            crate::build_all_in_one_assembly(
                &deployment,
                seal_key.as_ref().expect("AllInOne owns Control seal key"),
            )
            .await?
        }
        ServiceRole::Control => {
            crate::build_control_assembly(
                &deployment,
                seal_key.as_ref().expect("Control owns Control seal key"),
            )
            .await?
        }
        ServiceRole::Coordinator => crate::build_coordinator_assembly(&deployment).await?,
    };
    let local_setup = assembly.local_setup;
    let registration_supervisor = assembly.registration_supervisor;
    let app = assembly.router.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let controller = crate::DrainController::new();
    if let Some(supervisor) = registration_supervisor {
        controller.set_registration_supervisor(supervisor);
    }
    let _active_streams_gauge = crate::register_active_streams_gauge(controller.clone());
    let app = match &deployment.admin_listen {
        Some(_) => crate::with_connection_metric(app, controller.clone()),
        None => crate::with_process_admin(app, controller.clone()),
    };
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    let app = crate::mount_console_with_navigation(
        app,
        awaken_api_contract::SuiteNavigation {
            hub_url: deployment.suite_hub_url.clone(),
        },
    );

    if let Some(admin_addr) = &deployment.admin_listen {
        let listener = tokio::net::TcpListener::bind(admin_addr)
            .await
            .map_err(|error| friendly_bind_error("admin", admin_addr, error))?;
        let admin = crate::process_admin_router(controller);
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
    if role == ServiceRole::AllInOne {
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
    } else {
        eprintln!(
            "{}: ready url={url} data_dir={} mode={}",
            role.binary(),
            deployment.data_dir.display(),
            deployment.mode.as_str()
        );
        if let Some(setup) = &local_setup {
            eprintln!(
                "{}: local setup token={} expires_at={}",
                role.binary(),
                setup.setup_token,
                setup.expires_at.0
            );
        }
    }

    let Some(worker) = local_worker else {
        return axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|error| format!("server stopped: {error}"));
    };

    // AllInOne owns one shutdown sequence. The local Worker fences admission
    // and deregisters before the co-located HTTP service stops.
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
    fn service_roles_map_to_exact_deployment_authorities() {
        // Cause/effect decision table: S1 AllInOne -> union composition and
        // `awaken`; S2 Control -> Control authority and role binary; S3
        // Coordinator -> Coordinator authority and role binary. Worker has no
        // representable ServiceRole and therefore cannot enter this lifecycle.
        assert_eq!(
            ServiceRole::AllInOne.deployment_role(),
            Role::AllInOne,
            "S1"
        );
        assert_eq!(ServiceRole::AllInOne.binary(), "awaken", "S1");
        assert_eq!(ServiceRole::Control.deployment_role(), Role::Control, "S2");
        assert_eq!(ServiceRole::Control.binary(), "awaken-control", "S2");
        assert_eq!(
            ServiceRole::Coordinator.deployment_role(),
            Role::Coordinator,
            "S3"
        );
        assert_eq!(
            ServiceRole::Coordinator.binary(),
            "awaken-coordinator",
            "S3"
        );
    }

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
    fn bind_projection_and_conflict_message_cover_terminal_effects() {
        // Cause/effect rules: B1 unspecified IPv4/IPv6 binds project to their
        // loopback browser URLs; B2 an occupied address produces an actionable
        // terminal error naming the override. Other I/O errors preserve cause.
        assert_eq!(
            browser_url("0.0.0.0:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(browser_url("[::]:8080").unwrap(), "http://[::1]:8080");
        let message = friendly_bind_error(
            "server",
            "127.0.0.1:8080",
            std::io::Error::from(std::io::ErrorKind::AddrInUse),
        );
        assert!(message.contains("already in use"));
        assert!(message.contains("--port"));
    }
}
