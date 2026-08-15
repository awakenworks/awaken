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
    fn startup_role(self) -> awaken_service_lifecycle::StartupRole {
        match self {
            Self::AllInOne => awaken_service_lifecycle::StartupRole::AllInOne,
            Self::Control => awaken_service_lifecycle::StartupRole::Control,
            Self::Coordinator => awaken_service_lifecycle::StartupRole::Coordinator,
        }
    }

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

/// Run the exact Control, Coordinator, or AllInOne startup selected by the
/// caller. Role-specific binaries and `awaken <role>` both terminate here.
pub async fn run_service(args: ServiceArgs, role: ServiceRole) -> Result<(), String> {
    awaken_credential_materializer::reject_ambient_api_key_environment()
        .map_err(|error| error.to_string())?;
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
    let local_worker = if awaken_service_lifecycle::startup_requires(
        role.startup_role(),
        awaken_service_lifecycle::StartupComponent::LocalWorker,
    ) {
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

/// Serve a caller-configured Control process through Awaken's canonical public,
/// private, admin, health, failure-observation, and graceful-drain lifecycle.
///
/// Closed deployments may inject publication SPIs while retaining this single
/// service lifecycle and listener owner.
pub async fn serve_prepared_control(
    deployment: ResolvedDeployment,
    process: crate::PreparedProcess,
) -> Result<(), String> {
    validate_prebuilt_control_deployment(&deployment)?;
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    awaken_observability::init(&deployment.observability);
    let result = serve_prepared_process(deployment, ServiceRole::Control, process, None).await;
    awaken_observability::shutdown();
    result
}

/// Serve a caller-configured canonical Coordinator through the same lifecycle
/// and listener owner as the role binary.
pub async fn serve_prepared_coordinator(
    deployment: ResolvedDeployment,
    process: crate::PreparedProcess,
) -> Result<(), String> {
    if deployment.role != Role::Coordinator {
        return Err("prebuilt Coordinator process requires role = \"coordinator\"".into());
    }
    if deployment.mode != crate::config::OperatingMode::Server {
        return Err("prebuilt Coordinator process requires mode = \"server\"".into());
    }
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    awaken_observability::init(&deployment.observability);
    let result = serve_prepared_process(deployment, ServiceRole::Coordinator, process, None).await;
    awaken_observability::shutdown();
    result
}

fn validate_prebuilt_control_deployment(deployment: &ResolvedDeployment) -> Result<(), String> {
    if deployment.role != Role::Control {
        return Err("prebuilt Control process requires role = \"control\"".into());
    }
    if deployment.mode != crate::config::OperatingMode::Server {
        return Err("prebuilt Control process requires mode = \"server\"".into());
    }
    Ok(())
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
    let process = match role {
        ServiceRole::AllInOne => {
            crate::prepare_all_in_one_process(
                &deployment,
                seal_key.as_ref().expect("AllInOne owns Control seal key"),
            )
            .await?
        }
        ServiceRole::Control => {
            crate::prepare_control_process(
                &deployment,
                seal_key.as_ref().expect("Control owns Control seal key"),
            )
            .await?
        }
        ServiceRole::Coordinator => crate::prepare_coordinator_process(&deployment).await?,
    };
    serve_prepared_process(deployment, role, process, prepared_worker).await
}

async fn serve_prepared_process(
    deployment: ResolvedDeployment,
    role: ServiceRole,
    process: crate::PreparedProcess,
    prepared_worker: Option<crate::PreparedLocalWorker>,
) -> Result<(), String> {
    let local_setup = process.local_setup;
    let registration_supervisor = process.registration_supervisor;
    let service_lifecycle = process.service_lifecycle;
    let public_app = process.public_router.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let private_app = process.private_router;
    let controller = crate::DrainController::new();
    controller.set_service_lifecycle(service_lifecycle.clone());
    if let Some(supervisor) = registration_supervisor {
        controller.set_registration_supervisor(supervisor);
    }
    let _active_streams_gauge = crate::register_active_streams_gauge(controller.clone());
    let public_app = match &deployment.admin_listen {
        Some(_) => crate::with_connection_metric(public_app, controller.clone()),
        None => crate::with_process_admin(public_app, controller.clone()),
    };
    let public_app = public_app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    let public_app = crate::mount_console_with_navigation(
        public_app,
        awaken_api_contract::SuiteNavigation {
            hub_url: deployment.suite_hub_url.clone(),
        },
    );
    let private_app =
        private_app.layer(axum::middleware::from_fn(awaken_observability::trace_http));

    let admin_listener = match &deployment.admin_listen {
        Some(admin_addr) => Some(
            tokio::net::TcpListener::bind(admin_addr)
                .await
                .map_err(|error| friendly_bind_error("admin", admin_addr, error))?,
        ),
        None => None,
    };
    let (public_listener, private_listener) = bind_application_listeners(&deployment).await?;

    if let (Some(listener), Some(admin_addr)) = (admin_listener, &deployment.admin_listen) {
        let admin = crate::process_admin_router(controller);
        eprintln!("awaken: admin http://{admin_addr} (/readyz /metrics /admin/drain)");
        service_lifecycle.spawn("service-admin-http", move |cancel| async move {
            axum::serve(listener, admin)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
                .map_err(|error| format!("admin server stopped: {error}"))
        });
    }

    let url = browser_url(&deployment.bind)?;
    let worker_url = private_listener.as_ref().map(|listener| {
        format!(
            "http://{}",
            listener.local_addr().expect("bound listener address")
        )
    });
    let local_worker = prepared_worker
        .map(|prepared| {
            prepared.build_worker(
                worker_url
                    .clone()
                    .expect("AllInOne always binds its private Worker surface"),
                &deployment,
            )
        })
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
        eprintln!(
            "{}: private http://{}",
            role.binary(),
            deployment
                .internal_bind
                .as_deref()
                .expect("split service role requires internal_bind")
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
        let observed_failure = std::sync::Arc::new(std::sync::Mutex::new(None));
        let failure_slot = observed_failure.clone();
        let failure_source = service_lifecycle.clone();
        let server_result = serve_application_surfaces(
            public_listener,
            public_app,
            private_listener.map(|listener| (listener, private_app)),
            async move {
                tokio::select! {
                    () = shutdown_signal() => {}
                    failure = failure_source.wait_for_failure() => {
                        *failure_slot.lock().expect("process failure lock poisoned") = failure;
                    }
                }
            },
        )
        .await;
        let drain_result = service_lifecycle
            .shutdown(std::time::Duration::from_secs(10))
            .await
            .map_err(|error| error.to_string());
        server_result?;
        drain_result?;
        if let Some(failure) = observed_failure
            .lock()
            .expect("process failure lock poisoned")
            .take()
        {
            return Err(format!(
                "critical process task `{}` stopped: {}",
                failure.task, failure.cause
            ));
        }
        return Ok(());
    };

    // AllInOne owns one shutdown sequence. The local Worker fences admission
    // and deregisters before the co-located HTTP service stops.
    let (worker_shutdown_tx, worker_shutdown_rx) = tokio::sync::oneshot::channel();
    let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = serve_application_surfaces(
        public_listener,
        public_app,
        private_listener.map(|listener| (listener, private_app)),
        async move {
            let _ = server_shutdown_rx.await;
        },
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
    let result = tokio::select! {
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
        failure = service_lifecycle.wait_for_failure() => {
            if let Some(tx) = worker_shutdown_tx.take() {
                let _ = tx.send(awaken_worker::WorkerShutdown::Prompt);
            }
            let worker_result = local_worker_result((&mut worker).await);
            if let Some(tx) = server_shutdown_tx.take() {
                let _ = tx.send(());
            }
            let server_result = (&mut server)
                .await
                .map_err(|error| format!("server stopped: {error}"));
            worker_result?;
            server_result?;
            let failure = failure.expect("process task wait ends only for failure before cancellation");
            Err(format!(
                "critical process task `{}` stopped: {}",
                failure.task, failure.cause
            ))
        }
    };
    let drain_result = service_lifecycle
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .map_err(|error| error.to_string());
    result?;
    drain_result
}

async fn bind_application_listeners(
    deployment: &ResolvedDeployment,
) -> Result<(tokio::net::TcpListener, Option<tokio::net::TcpListener>), String> {
    let public = tokio::net::TcpListener::bind(&deployment.bind)
        .await
        .map_err(|error| friendly_bind_error("public server", &deployment.bind, error))?;
    let private = match deployment.internal_bind.as_deref() {
        Some(address) => Some(
            tokio::net::TcpListener::bind(address)
                .await
                .map_err(|error| friendly_bind_error("private server", address, error))?,
        ),
        None => None,
    };
    Ok((public, private))
}

async fn wait_for_surface_shutdown(mut receiver: tokio::sync::watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    let _ = receiver.changed().await;
}

async fn serve_application_surfaces<Shutdown>(
    public_listener: tokio::net::TcpListener,
    public_app: axum::Router,
    private: Option<(tokio::net::TcpListener, axum::Router)>,
    shutdown: Shutdown,
) -> Result<(), String>
where
    Shutdown: std::future::Future<Output = ()> + Send + 'static,
{
    let Some((private_listener, private_app)) = private else {
        return axum::serve(public_listener, public_app)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|error| format!("public server stopped: {error}"));
    };

    let (stop, receiver) = tokio::sync::watch::channel(false);
    let public = std::future::IntoFuture::into_future(
        axum::serve(public_listener, public_app)
            .with_graceful_shutdown(wait_for_surface_shutdown(receiver.clone())),
    );
    let private = std::future::IntoFuture::into_future(
        axum::serve(private_listener, private_app)
            .with_graceful_shutdown(wait_for_surface_shutdown(receiver)),
    );
    tokio::pin!(public);
    tokio::pin!(private);
    tokio::pin!(shutdown);

    tokio::select! {
        _ = &mut shutdown => {
            let _ = stop.send(true);
            let (public, private) = tokio::join!(&mut public, &mut private);
            public.map_err(|error| format!("public server stopped: {error}"))?;
            private.map_err(|error| format!("private server stopped: {error}"))
        }
        public = &mut public => {
            let _ = stop.send(true);
            let private = (&mut private).await;
            public.map_err(|error| format!("public server stopped: {error}"))?;
            private.map_err(|error| format!("private server stopped: {error}"))
        }
        private = &mut private => {
            let _ = stop.send(true);
            let public = (&mut public).await;
            private.map_err(|error| format!("private server stopped: {error}"))?;
            public.map_err(|error| format!("public server stopped: {error}"))
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
        if kind == "private server" {
            return format!(
                "cannot start {kind} on {address}: the address is already in use; choose another internal_bind in the deployment config"
            );
        }
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

    #[tokio::test]
    async fn public_and_private_servers_share_one_graceful_lifecycle() {
        // Cause/effect graph: C1 both listeners bind, C2 each owns its distinct
        // Router, C3 the shared shutdown resolves. Effects: E1 both sockets
        // accept while the process is live; E2 one shutdown drains and closes
        // both; E3 the lifecycle future terminates successfully.
        // Decision table: R1 C1+C2+!C3 -> E1; R2 C1+C2+C3 -> E2+E3. Router
        // path isolation and authentication are owned by the role-surface test.
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_addr = public.local_addr().unwrap();
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let private_addr = private.local_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let lifecycle = tokio::spawn(serve_application_surfaces(
            public,
            axum::Router::new(),
            Some((private, axum::Router::new())),
            async move {
                let _ = stopped.await;
            },
        ));

        let public_connection = tokio::net::TcpStream::connect(public_addr).await;
        let private_connection = tokio::net::TcpStream::connect(private_addr).await;
        assert!(public_connection.is_ok(), "R1 public");
        assert!(private_connection.is_ok(), "R1 private");
        drop(public_connection);
        drop(private_connection);

        shutdown.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), lifecycle)
            .await
            .expect("R2 both listeners drain")
            .expect("R3 lifecycle task")
            .expect("R3 lifecycle result");
        assert!(
            tokio::net::TcpStream::connect(public_addr).await.is_err(),
            "R2 public closed"
        );
        assert!(
            tokio::net::TcpStream::connect(private_addr).await.is_err(),
            "R2 private closed"
        );
    }

    #[tokio::test]
    async fn private_bind_failure_aborts_before_any_surface_is_served() {
        // Cause/effect graph: C1 public and private addresses alias, C2 public
        // bind succeeds first, C3 private bind conflicts. Effect E1 startup
        // fails with the private boundary named and the unserved public listener
        // is dropped. Decision rule B1=C1+C2+C3 -> E1. Valid dual-bind startup
        // is covered by the shared-lifecycle test.
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let mut deployment =
            crate::config::local_test_deployment(tempfile::tempdir().unwrap().path().to_owned());
        deployment.bind = address.to_string();
        deployment.internal_bind = Some(address.to_string());
        let error = bind_application_listeners(&deployment)
            .await
            .expect_err("B1 aliased private bind must fail");
        assert!(error.contains("private server"), "B1: {error}");
        assert!(error.contains("already in use"), "B1: {error}");
        assert!(
            tokio::net::TcpListener::bind(address).await.is_ok(),
            "E1 public listener was never served and was dropped"
        );
    }

    #[test]
    fn service_roles_map_to_exact_deployment_authorities() {
        // Cause/effect decision table: S1 AllInOne -> union startup and
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
    fn prebuilt_control_lifecycle_rejects_mismatched_authority_or_mode() {
        // Cause/effect decision table: P1 Control+server is the only accepted
        // prebuilt authority; P2 AllInOne+server is rejected before serving;
        // P3 Control+local is rejected before serving. This prevents an injected
        // publication adapter from widening either the role or deployment mode.
        let dir = tempfile::tempdir().unwrap();
        let mut deployment =
            crate::config::local_test_deployment(dir.path().join("prebuilt-control"));
        deployment.role = Role::Control;
        deployment.mode = crate::config::OperatingMode::Server;
        assert!(
            validate_prebuilt_control_deployment(&deployment).is_ok(),
            "P1"
        );

        deployment.role = Role::AllInOne;
        assert!(
            validate_prebuilt_control_deployment(&deployment)
                .unwrap_err()
                .contains("role"),
            "P2"
        );

        deployment.role = Role::Control;
        deployment.mode = crate::config::OperatingMode::Local;
        assert!(
            validate_prebuilt_control_deployment(&deployment)
                .unwrap_err()
                .contains("mode"),
            "P3"
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
internal_bind = "127.0.0.1:8081"
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
