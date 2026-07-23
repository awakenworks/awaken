use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Server,
    Console,
    Help,
}

pub(crate) fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Mode, String> {
    let args: Vec<String> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Mode::Server),
        [command] if command == "start" => Ok(Mode::Console),
        [flag] if flag == "-h" || flag == "--help" => Ok(Mode::Help),
        [command, flag] if command == "start" && (flag == "-h" || flag == "--help") => {
            Ok(Mode::Help)
        }
        _ => Err(format!(
            "unknown arguments: {}; run `awaken --help`",
            args.join(" ")
        )),
    }
}

pub(crate) fn print_help() {
    println!(
        "Awaken\n\nUSAGE:\n    awaken          Start the API server\n    awaken start    Build and start the API plus web console\n\nENVIRONMENT:\n    AWAKEN_HTTP_ADDR   Listen address (default 127.0.0.1:8080)\n    AWAKEN_WEB_DIST    Prebuilt console dist directory"
    );
}

pub(crate) fn prepare_dist() -> Result<PathBuf, String> {
    if let Some(dist) = std::env::var_os("AWAKEN_WEB_DIST").map(PathBuf::from) {
        return validate_dist(dist);
    }
    let web = find_web_dir().ok_or_else(|| {
        "could not locate web/package.json; set AWAKEN_WEB_DIST to a built console directory"
            .to_string()
    })?;
    let dist = web.join("dist");
    if !dist.join("index.html").is_file() {
        build_console(&web)?;
    }
    validate_dist(dist)
}

fn validate_dist(dist: PathBuf) -> Result<PathBuf, String> {
    if dist.join("index.html").is_file() {
        dist.canonicalize()
            .map_err(|error| format!("resolve console dist {}: {error}", dist.display()))
    } else {
        Err(format!(
            "console dist {} does not contain index.html",
            dist.display()
        ))
    }
}

fn find_web_dir() -> Option<PathBuf> {
    let mut starts = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        starts.push(current);
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        starts.push(parent.to_path_buf());
    }
    starts.into_iter().find_map(|start| {
        start.ancestors().find_map(|ancestor| {
            let candidate = ancestor.join("web");
            candidate
                .join("package.json")
                .is_file()
                .then_some(candidate)
        })
    })
}

fn build_console(web: &Path) -> Result<(), String> {
    let package_manager = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    if !web.join("node_modules").is_dir() {
        run_pnpm(package_manager, web, &["install", "--frozen-lockfile"])?;
    }
    run_pnpm(package_manager, web, &["build"])
}

fn run_pnpm(package_manager: &str, web: &Path, args: &[&str]) -> Result<(), String> {
    eprintln!(
        "awaken console: running {package_manager} {}",
        args.join(" ")
    );
    let status = Command::new(package_manager)
        .current_dir(web)
        .args(args)
        .status()
        .map_err(|error| format!("start {package_manager}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{package_manager} {} exited with {status}",
            args.join(" ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_modes_are_explicit() {
        assert_eq!(parse_args(Vec::new()).unwrap(), Mode::Server);
        assert_eq!(parse_args(["start".into()]).unwrap(), Mode::Console);
        assert_eq!(parse_args(["--help".into()]).unwrap(), Mode::Help);
        assert!(parse_args(["serve".into()]).is_err());
    }

    #[test]
    fn a_dist_requires_an_index() {
        let temp = tempfile::tempdir().unwrap();
        assert!(validate_dist(temp.path().to_path_buf()).is_err());
        std::fs::write(temp.path().join("index.html"), "ready").unwrap();
        assert!(validate_dist(temp.path().to_path_buf()).is_ok());
    }
}
