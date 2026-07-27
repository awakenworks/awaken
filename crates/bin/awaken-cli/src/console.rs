use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;

include!(concat!(env!("OUT_DIR"), "/embedded_console.rs"));

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StartArgs {
    pub config_path: Option<std::path::PathBuf>,
    pub port: Option<u16>,
    pub data_dir: Option<std::path::PathBuf>,
    pub no_browser: bool,
    pub identity_mode: Option<awaken_control::ManagementIdentityMode>,
    pub cloud_models: Option<awaken_cli::config::CloudModelMode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Start(StartArgs),
    Serve(StartArgs),
    Management(StartArgs),
    DatabaseMigrate {
        config_path: Option<std::path::PathBuf>,
    },
    Worker {
        server: String,
        config_path: Option<std::path::PathBuf>,
    },
    Config {
        json: bool,
        config_path: Option<std::path::PathBuf>,
    },
    DoctorAcp {
        json: bool,
    },
    Version,
    Help,
}

pub(crate) fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter().collect::<Vec<_>>();
    if args.is_empty() {
        return Ok(Command::Start(StartArgs::default()));
    }
    let command = args.remove(0);
    match command.as_str() {
        "start" if args.iter().any(|arg| is_help(arg)) => Ok(Command::Help),
        "start" => parse_start_args(&args).map(Command::Start),
        "serve" if args.iter().any(|arg| is_help(arg)) => Ok(Command::Help),
        "serve" => parse_start_args(&args).map(Command::Serve),
        "management" if args.iter().any(|arg| is_help(arg)) => Ok(Command::Help),
        "management" => parse_start_args(&args).map(Command::Management),
        "database" => parse_database_args(&args),
        "worker" => parse_worker_args(&args),
        "config" => parse_config_args(&args),
        "doctor" => parse_doctor_args(&args),
        "version" | "-V" | "--version" if args.is_empty() => Ok(Command::Version),
        "help" | "-h" | "--help" if args.is_empty() => Ok(Command::Help),
        other => Err(format!("unknown command {other:?}; run `awaken --help`")),
    }
}

fn parse_doctor_args(args: &[String]) -> Result<Command, String> {
    let Some((subject, options)) = args.split_first() else {
        return Err("doctor requires the 'acp' subject".to_owned());
    };
    if subject != "acp" {
        return Err(format!(
            "unknown doctor subject {subject:?}; expected 'acp'"
        ));
    }
    if options.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    let mut json = false;
    for option in options {
        match option.as_str() {
            "--json" => json = true,
            other => return Err(format!("unexpected doctor acp argument {other:?}")),
        }
    }
    Ok(Command::DoctorAcp { json })
}

fn parse_start_args(args: &[String]) -> Result<StartArgs, String> {
    let mut parsed = StartArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--no-browser" => parsed.no_browser = true,
            "--identity-mode" => {
                index += 1;
                parsed.identity_mode =
                    Some(parse_identity_mode(args.get(index).map(String::as_str))?);
            }
            value if value.starts_with("--identity-mode=") => {
                parsed.identity_mode = Some(parse_identity_mode(Some(&value[16..]))?);
            }
            "--cloud-models" => {
                index += 1;
                parsed.cloud_models =
                    Some(parse_cloud_models(args.get(index).map(String::as_str))?);
            }
            value if value.starts_with("--cloud-models=") => {
                parsed.cloud_models = Some(parse_cloud_models(Some(&value[15..]))?);
            }
            "--config" => {
                index += 1;
                parsed.config_path =
                    Some(parse_path(args.get(index).map(String::as_str), "--config")?);
            }
            value if value.starts_with("--config=") => {
                parsed.config_path = Some(parse_path(Some(&value[9..]), "--config")?);
            }
            "--port" => {
                index += 1;
                parsed.port = Some(parse_port(args.get(index).map(String::as_str))?);
            }
            value if value.starts_with("--port=") => {
                parsed.port = Some(parse_port(Some(&value[7..]))?);
            }
            "--data-dir" => {
                index += 1;
                parsed.data_dir = Some(parse_path(
                    args.get(index).map(String::as_str),
                    "--data-dir",
                )?);
            }
            value if value.starts_with("--data-dir=") => {
                parsed.data_dir = Some(parse_path(Some(&value[11..]), "--data-dir")?);
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_worker_args(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    let mut server = None;
    let mut config_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--server" => {
                index += 1;
                server = args.get(index).cloned();
            }
            value if value.starts_with("--server=") => server = Some(value[9..].to_owned()),
            "--config" => {
                index += 1;
                config_path = Some(parse_path(args.get(index).map(String::as_str), "--config")?);
            }
            value if value.starts_with("--config=") => {
                config_path = Some(parse_path(Some(&value[9..]), "--config")?);
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
        index += 1;
    }
    let server = server.ok_or_else(|| "worker requires --server <URL>".to_owned())?;
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        return Err("--server must be an http:// or https:// URL".to_owned());
    }
    Ok(Command::Worker {
        server,
        config_path,
    })
}

fn parse_config_args(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    let mut json = false;
    let mut config_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--config" => {
                index += 1;
                config_path = Some(parse_path(args.get(index).map(String::as_str), "--config")?);
            }
            value if value.starts_with("--config=") => {
                config_path = Some(parse_path(Some(&value[9..]), "--config")?);
            }
            other => return Err(format!("unexpected config argument {other:?}")),
        }
        index += 1;
    }
    Ok(Command::Config { json, config_path })
}

fn parse_database_args(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    let Some((subcommand, args)) = args.split_first() else {
        return Err("database requires the `migrate` subcommand".to_owned());
    };
    if subcommand != "migrate" {
        return Err("database requires the `migrate` subcommand".to_owned());
    }
    let mut config_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                config_path = Some(parse_path(args.get(index).map(String::as_str), "--config")?);
            }
            value if value.starts_with("--config=") => {
                config_path = Some(parse_path(Some(&value[9..]), "--config")?);
            }
            other => return Err(format!("unexpected database migrate argument {other:?}")),
        }
        index += 1;
    }
    Ok(Command::DatabaseMigrate { config_path })
}

fn parse_port(value: Option<&str>) -> Result<u16, String> {
    value
        .ok_or_else(|| "--port needs a value".to_owned())?
        .parse::<u16>()
        .map_err(|_| "--port must be an integer from 1 to 65535".to_owned())
        .and_then(|port| {
            (port != 0)
                .then_some(port)
                .ok_or_else(|| "--port must not be 0".to_owned())
        })
}

fn parse_identity_mode(
    value: Option<&str>,
) -> Result<awaken_control::ManagementIdentityMode, String> {
    value
        .and_then(awaken_control::ManagementIdentityMode::parse)
        .ok_or_else(|| "--identity-mode expects no-login, self-managed, or awaken-cloud".to_owned())
}

fn parse_cloud_models(value: Option<&str>) -> Result<awaken_cli::config::CloudModelMode, String> {
    awaken_cli::config::CloudModelMode::parse(
        value.ok_or_else(|| "--cloud-models needs disabled or enabled".to_owned())?,
    )
}

fn parse_path(value: Option<&str>, flag: &str) -> Result<std::path::PathBuf, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a non-empty path"))
}

fn is_help(value: &str) -> bool {
    value == "-h" || value == "--help"
}

pub(crate) fn print_help() {
    println!(
        "Awaken\n\nUSAGE:\n    awaken [COMMAND] [OPTIONS]\n\nRunning `awaken` without a command is the same as `awaken start`.\n\nCOMMANDS:\n    start                 Start locally, print readiness, and open the browser\n    serve                 Start headless for service managers\n    management            Start only the authoring/control surface (server mode)\n    database migrate      Apply management schema migrations and exit\n    worker --server URL   Join an Awaken server as a worker\n    doctor acp [--json]   Discover and diagnose supported local ACP agents\n    config [--json]       Print effective, redacted configuration\n    version               Print the installed version\n\nOPTIONS:\n    --config PATH         Read typed configuration from PATH\n    --port PORT           Override the listen port\n    --data-dir PATH       Override the persistent data root (default ~/.awaken)\n    --no-browser          Do not open a browser\n    --identity-mode MODE  no-login, self-managed, or awaken-cloud\n    --cloud-models MODE   disabled or enabled (requires awaken-cloud identity)\n    -h, --help            Print this help\n\nConfiguration sources: --config PATH or ~/.awaken/config.toml, then defaults."
    );
}

pub(crate) fn mount(app: Router) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/w/{*path}", get(index))
        .route("/assets/{*path}", get(asset))
        .fallback_service(app)
}

async fn index() -> Response {
    response("index.html", false)
}

async fn asset(Path(path): Path<String>) -> Response {
    response(&format!("assets/{path}"), true)
}

fn response(path: &str, immutable: bool) -> Response {
    let Some(bytes) = embedded_asset(path) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("valid not-found response");
    };
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(bytes))
        .expect("valid embedded asset response")
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::put;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use super::*;

    #[test]
    fn command_line_modes_are_explicit() {
        assert_eq!(
            parse_args(Vec::new()).unwrap(),
            Command::Start(StartArgs::default())
        );
        assert_eq!(
            parse_args(["serve".into()]).unwrap(),
            Command::Serve(StartArgs::default())
        );
        assert_eq!(
            parse_args(["management".into()]).unwrap(),
            Command::Management(StartArgs::default())
        );
        assert_eq!(
            parse_args(["database".into(), "migrate".into()]).unwrap(),
            Command::DatabaseMigrate { config_path: None }
        );
        assert_eq!(
            parse_args([
                "database".into(),
                "migrate".into(),
                "--config".into(),
                "/etc/awaken/config.toml".into(),
            ])
            .unwrap(),
            Command::DatabaseMigrate {
                config_path: Some("/etc/awaken/config.toml".into())
            }
        );
        assert_eq!(parse_args(["--help".into()]).unwrap(), Command::Help);
        assert_eq!(
            parse_args(["start".into(), "--port".into(), "9123".into()]).unwrap(),
            Command::Start(StartArgs {
                port: Some(9123),
                ..Default::default()
            })
        );
        assert_eq!(
            parse_args([
                "serve".into(),
                "--identity-mode=awaken-cloud".into(),
                "--cloud-models".into(),
                "enabled".into(),
            ])
            .unwrap(),
            Command::Serve(StartArgs {
                identity_mode: Some(awaken_control::ManagementIdentityMode::AwakenCloud),
                cloud_models: Some(awaken_cli::config::CloudModelMode::Enabled),
                ..Default::default()
            })
        );
        assert!(parse_args(["worker".into()]).is_err());
        assert_eq!(
            parse_args(["doctor".into(), "acp".into(), "--json".into()]).unwrap(),
            Command::DoctorAcp { json: true }
        );
    }

    #[tokio::test]
    async fn embedded_console_serves_the_spa_and_preserves_the_api() {
        let api = Router::new().route("/v1/probe", put(|| async { StatusCode::CREATED }));
        let app = mount(api);

        let index = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(
            index.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = index.into_body().collect().await.unwrap().to_bytes();
        assert!(
            body.windows(b"Awaken Console".len())
                .any(|window| { window == b"Awaken Console" })
        );

        let api = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/v1/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::CREATED);
    }
}
