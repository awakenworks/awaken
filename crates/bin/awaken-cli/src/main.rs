//! `awaken` — one product command and one serve composition.

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
        console::Command::DatabaseMigrate { config_path } => {
            let deployment = ResolvedDeployment::load(ConfigOverrides {
                config_path,
                role: Some(Role::Serve),
                ..Default::default()
            })?;
            warn_deprecations(&deployment);
            deployment.ensure_data_layout()?;
            let seal_key = deployment.seal_key.load_or_create()?;
            awaken_cli::migrate_management_schema_with_deployment(&deployment, &seal_key).await
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
            let key = deployment.seal_key.load_or_create()?;
            let resource_url = match deployment.resources {
                awaken_cli::config::ResourcePlaneStoreBackend::Postgres(url) => Some(url),
                awaken_cli::config::ResourcePlaneStoreBackend::Embedded(_) => None,
            };
            let worker = deployment.worker;
            let mut manifest = worker
                .build_digest
                .map(awaken_worker::StandardManifestConfig::new)
                .unwrap_or_default()
                .with_extra_capabilities(worker.capabilities);
            if let Some(zone) = worker.zone {
                manifest = manifest.with_zone(zone);
            }
            if let Some(max_concurrent) = worker.max_concurrent {
                manifest = manifest.with_max_concurrent(max_concurrent);
            }
            awaken_observability::init();
            let result = awaken_worker::run_with_config(
                &server,
                deployment.runtime,
                deployment.control,
                resource_url,
                key,
                awaken_worker::WorkerRunOptions {
                    admin_listen: worker.admin_listen,
                    drain_grace: std::time::Duration::from_secs(worker.drain_grace_secs),
                    credential_probe_interval: std::time::Duration::from_secs(
                        worker.credential_probe_interval_secs,
                    ),
                    credential_observation_ttl: std::time::Duration::from_secs(
                        worker.credential_observation_ttl_secs,
                    ),
                    manifest,
                },
            )
            .await
            .map_err(|error| error.to_string());
            awaken_observability::shutdown();
            result
        }
        console::Command::Start(args) => serve(args, Presentation::Interactive, false).await,
        console::Command::Serve(args) => serve(args, Presentation::Headless, false).await,
        console::Command::Management(args) => serve(args, Presentation::Headless, true).await,
    }
}

async fn serve(
    args: console::StartArgs,
    presentation: Presentation,
    management_only: bool,
) -> Result<(), String> {
    let deployment = ResolvedDeployment::load(ConfigOverrides {
        config_path: args.config_path,
        role: Some(Role::Serve),
        data_dir: args.data_dir,
        port: args.port,
        no_browser: args.no_browser.then_some(true),
        identity_mode: args.identity_mode,
        cloud_models: args.cloud_models,
        ..Default::default()
    })?;
    if management_only && deployment.mode != awaken_cli::config::OperatingMode::Server {
        return Err("`awaken management` requires mode = \"server\" in config.toml".to_owned());
    }
    warn_deprecations(&deployment);
    deployment.ensure_data_layout()?;
    let seal_key = deployment.seal_key.load_or_create()?;
    if !management_only
        && let Some(error) = deployment.runtime.durable_needs_persistence_error(false)
    {
        return Err(error.to_owned());
    }

    awaken_observability::init();
    let result = serve_resolved(deployment, seal_key, presentation, management_only).await;
    awaken_observability::shutdown();
    result
}

async fn serve_resolved(
    deployment: ResolvedDeployment,
    seal_key: [u8; 32],
    presentation: Presentation,
    management_only: bool,
) -> Result<(), String> {
    let postgres_startup = !management_only
        && (deployment.runtime.dispatch_backend == awaken_runtime_host::DispatchBackend::Postgres
            || deployment.runtime.store == awaken_runtime_host::StoreKind::Postgres);
    let migration_lock = if postgres_startup {
        let url = deployment.runtime.database_url.as_deref().ok_or_else(|| {
            "a Postgres runtime requires runtime.database_url in the deployment config".to_owned()
        })?;
        Some(
            awaken_runtime_host::PostgresMigrationLock::acquire(url)
                .await
                .map_err(|error| format!("acquire database migration lock: {error}"))?,
        )
    } else {
        None
    };

    if !management_only
        && deployment.runtime.dispatch_backend == awaken_runtime_host::DispatchBackend::Postgres
    {
        let url = deployment
            .runtime
            .database_url
            .as_deref()
            .expect("validated Postgres dispatch URL");
        awaken_runtime_host::init_shared_postgres_dispatch_with_config(url, &deployment.runtime)
            .await
            .map_err(|error| format!("initialize Postgres dispatch: {error}"))?;
        awaken_server::init_postgres_worker_registry(url)
            .await
            .map_err(|error| format!("initialize Postgres worker registry: {error}"))?;
    }
    if !management_only && deployment.runtime.store == awaken_runtime_host::StoreKind::Postgres {
        let url = deployment
            .runtime
            .database_url
            .as_deref()
            .expect("validated Postgres commit URL");
        awaken_runtime_host::init_shared_postgres_commit(url)
            .await
            .map_err(|error| format!("initialize Postgres commit store: {error}"))?;
    }

    let app = if management_only {
        awaken_cli::build_control_router_with_deployment(&deployment, &seal_key).await?
    } else {
        awaken_cli::build_management_router_with_deployment(&deployment, &seal_key).await?
    };
    if let Some(lock) = migration_lock {
        lock.release()
            .await
            .map_err(|error| format!("release database migration lock: {error}"))?;
    }

    let ctrl = awaken_cli::DrainController::new();
    let _active_streams_gauge = awaken_cli::register_active_streams_gauge(ctrl.clone());
    let app = match &deployment.admin_listen {
        Some(_) => awaken_cli::with_connection_metric(app, ctrl.clone()),
        None => awaken_cli::with_brain_admin(app, ctrl.clone()),
    };
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    let app = console::mount(app);

    if let Some(admin_addr) = &deployment.admin_listen {
        let listener = tokio::net::TcpListener::bind(admin_addr)
            .await
            .map_err(|error| friendly_bind_error("admin", admin_addr, error))?;
        let admin = awaken_cli::brain_admin_router(ctrl);
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
    match presentation {
        Presentation::Interactive => {
            eprintln!("\n  Awaken is ready\n");
            eprintln!("  Console   {url}");
            eprintln!("  Data      {}", deployment.data_dir.display());
            eprintln!("  Stop      Ctrl+C\n");
            if !deployment.no_browser
                && let Err(error) = open_browser(&url)
            {
                eprintln!("awaken: could not open a browser ({error}); open {url} manually");
            }
        }
        Presentation::Headless => eprintln!(
            "awaken: ready url={url} data_dir={} mode={}",
            deployment.data_dir.display(),
            deployment.mode.as_str()
        ),
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("server stopped: {error}"))
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

/// Resolve on SIGINT (developer stop) or SIGTERM (orchestrator stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
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
mod tests {
    use super::*;

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
