#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ServiceArgs {
    pub config_path: Option<std::path::PathBuf>,
    pub port: Option<u16>,
    pub data_dir: Option<std::path::PathBuf>,
    pub no_browser: bool,
    pub identity_mode: Option<awaken_control::ManagementIdentityMode>,
    pub cloud_models: Option<awaken_cli::config::CloudModelMode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    AllInOne(ServiceArgs),
    Control(ServiceArgs),
    Coordinator(ServiceArgs),
    ControlIamProfile,
    ControlIamResourceProfile,
    ControlIamRuntimeProfile,
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
        return Ok(Command::AllInOne(ServiceArgs::default()));
    }
    let command = args.remove(0);
    match command.as_str() {
        "all-in-one" if args.iter().any(|arg| is_help(arg)) => Ok(Command::Help),
        "all-in-one" => parse_service_args(&args).map(Command::AllInOne),
        "control" => parse_control_args(&args),
        "coordinator" if args.iter().any(|arg| is_help(arg)) => Ok(Command::Help),
        "coordinator" => parse_service_args(&args).map(Command::Coordinator),
        "database" => parse_database_args(&args),
        "worker" => parse_worker_args(&args),
        "config" => parse_config_args(&args),
        "doctor" => parse_doctor_args(&args),
        "version" | "-V" | "--version" if args.is_empty() => Ok(Command::Version),
        "help" | "-h" | "--help" if args.is_empty() => Ok(Command::Help),
        other => Err(format!("unknown command {other:?}; run `awaken --help`")),
    }
}

fn parse_control_args(args: &[String]) -> Result<Command, String> {
    if args == ["iam", "profile"] {
        return Ok(Command::ControlIamProfile);
    }
    if args == ["iam", "profile", "resources"] {
        return Ok(Command::ControlIamResourceProfile);
    }
    if args == ["iam", "profile", "runtime"] {
        return Ok(Command::ControlIamRuntimeProfile);
    }
    if args.first().is_some_and(|arg| arg == "iam") {
        return Err(
            "control iam requires `profile`, `profile resources`, or `profile runtime` exactly"
                .to_owned(),
        );
    }
    if args.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    parse_service_args(args).map(Command::Control)
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

fn parse_service_args(args: &[String]) -> Result<ServiceArgs, String> {
    let mut parsed = ServiceArgs::default();
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
        "Awaken\n\nUSAGE:\n    awaken [COMMAND] [OPTIONS]\n\nRunning `awaken` without a command is the same as `awaken all-in-one`.\n\nCOMMANDS:\n    all-in-one                      Run Control, Coordinator, and the local Worker together\n    control                         Run only the authoring and publication service\n    coordinator                     Run only Session, Run, Dispatch, and Worker coordination\n    control iam profile             Print the compiled Control IAM profile\n    control iam profile resources   Print the compiled Control resource IAM profile\n    control iam profile runtime     Print the compiled Hosted Runtime IAM profile\n    database migrate                Apply deployment schema migrations and exit\n    worker --server URL             Join a Coordinator as a Worker\n    doctor acp [--json]             Discover and diagnose supported local ACP agents\n    config [--json]                 Print effective, redacted configuration\n    version                         Print the installed version\n\nOPTIONS:\n    --config PATH         Read typed configuration from PATH\n    --port PORT           Override the listen port\n    --data-dir PATH       Override the persistent data root (default ~/.awaken)\n    --no-browser          Do not open the browser\n    --identity-mode MODE  no-login, self-managed, or awaken-cloud\n    --cloud-models MODE   disabled or enabled (requires awaken-cloud identity)\n    -h, --help            Print this help\n\nConfiguration sources: --config PATH or ~/.awaken/config.toml, then defaults."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_modes_are_explicit() {
        // Cause/effect decision table:
        // R1 no command -> the canonical all-in-one role; R2 each service name ->
        // exactly that service role; R3 Control IAM suffix -> the matching report;
        // R4 retired overlapping names -> reject instead of preserving a second
        // command path; R5 service options -> stay attached to the selected role.
        assert_eq!(
            parse_args(Vec::new()).unwrap(),
            Command::AllInOne(ServiceArgs::default())
        );
        assert_eq!(
            parse_args(["all-in-one".into()]).unwrap(),
            Command::AllInOne(ServiceArgs::default())
        );
        assert_eq!(
            parse_args(["control".into()]).unwrap(),
            Command::Control(ServiceArgs::default())
        );
        assert_eq!(
            parse_args(["coordinator".into()]).unwrap(),
            Command::Coordinator(ServiceArgs::default())
        );
        assert_eq!(
            parse_args(["control".into(), "iam".into(), "profile".into()]).unwrap(),
            Command::ControlIamProfile
        );
        assert_eq!(
            parse_args([
                "control".into(),
                "iam".into(),
                "profile".into(),
                "resources".into()
            ])
            .unwrap(),
            Command::ControlIamResourceProfile
        );
        assert_eq!(
            parse_args([
                "control".into(),
                "iam".into(),
                "profile".into(),
                "runtime".into()
            ])
            .unwrap(),
            Command::ControlIamRuntimeProfile
        );
        assert!(
            parse_args([
                "control".into(),
                "iam".into(),
                "profile".into(),
                "extra".into()
            ])
            .is_err()
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
            parse_args(["all-in-one".into(), "--port".into(), "9123".into()]).unwrap(),
            Command::AllInOne(ServiceArgs {
                port: Some(9123),
                ..Default::default()
            })
        );
        assert_eq!(
            parse_args([
                "control".into(),
                "--identity-mode=awaken-cloud".into(),
                "--cloud-models".into(),
                "enabled".into(),
            ])
            .unwrap(),
            Command::Control(ServiceArgs {
                identity_mode: Some(awaken_control::ManagementIdentityMode::AwakenCloud),
                cloud_models: Some(awaken_cli::config::CloudModelMode::Enabled),
                ..Default::default()
            })
        );
        for retired in ["start", "serve", "management"] {
            assert!(parse_args([retired.to_owned()]).is_err(), "R4 {retired}");
        }
        assert!(parse_args(["worker".into()]).is_err());
        assert_eq!(
            parse_args(["doctor".into(), "acp".into(), "--json".into()]).unwrap(),
            Command::DoctorAcp { json: true }
        );
    }
}
