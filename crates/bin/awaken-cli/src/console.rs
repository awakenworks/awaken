use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;

include!(concat!(env!("OUT_DIR"), "/embedded_console.rs"));

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StartArgs {
    pub port: Option<u16>,
    pub data_dir: Option<std::path::PathBuf>,
    pub no_browser: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Start(StartArgs),
    Serve(StartArgs),
    Management(StartArgs),
    Worker { server: String },
    Config { json: bool },
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
        "worker" => parse_worker_args(&args),
        "config" => match args.as_slice() {
            [] => Ok(Command::Config { json: false }),
            [flag] if flag == "--json" => Ok(Command::Config { json: true }),
            [flag] if is_help(flag) => Ok(Command::Help),
            _ => Err(format!("unexpected config arguments: {}", args.join(" "))),
        },
        "version" | "-V" | "--version" if args.is_empty() => Ok(Command::Version),
        "help" | "-h" | "--help" if args.is_empty() => Ok(Command::Help),
        other => Err(format!("unknown command {other:?}; run `awaken --help`")),
    }
}

fn parse_start_args(args: &[String]) -> Result<StartArgs, String> {
    let mut parsed = StartArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--no-browser" => parsed.no_browser = true,
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
    let server = match args {
        [flag, value] if flag == "--server" => value.clone(),
        [value] if value.starts_with("--server=") => value[9..].to_owned(),
        [flag] if is_help(flag) => return Ok(Command::Help),
        _ => return Err("worker requires --server <URL>".to_owned()),
    };
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        return Err("--server must be an http:// or https:// URL".to_owned());
    }
    Ok(Command::Worker { server })
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
        "Awaken\n\nUSAGE:\n    awaken [COMMAND] [OPTIONS]\n\nRunning `awaken` without a command is the same as `awaken start`.\n\nCOMMANDS:\n    start                 Start locally, print readiness, and open the browser\n    serve                 Start headless for service managers\n    management            Start only the authoring/control surface (server mode)\n    worker --server URL   Join an Awaken server as a worker\n    config [--json]       Print effective, redacted configuration\n    version               Print the installed version\n\nSTART / SERVE / MANAGEMENT OPTIONS:\n    --port PORT           Override the listen port\n    --data-dir PATH       Override the persistent data root (default ~/.awaken)\n    --no-browser          Do not open a browser\n    -h, --help            Print this help\n\nConfiguration precedence: command line > AWAKEN_* environment > config.toml > defaults."
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
        assert_eq!(parse_args(["--help".into()]).unwrap(), Command::Help);
        assert_eq!(
            parse_args(["start".into(), "--port".into(), "9123".into()]).unwrap(),
            Command::Start(StartArgs {
                port: Some(9123),
                ..Default::default()
            })
        );
        assert!(parse_args(["worker".into()]).is_err());
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
