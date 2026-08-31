#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceArgs {
    pub config_path: Option<std::path::PathBuf>,
    pub port: Option<u16>,
    pub data_dir: Option<std::path::PathBuf>,
    pub no_browser: bool,
    pub identity_mode: Option<awaken_control::ManagementIdentityMode>,
    pub cloud_models: Option<crate::config::CloudModelMode>,
}

/// One validated operator migration request shared by the product and
/// role-specific binaries. Each reference is present only when the operator
/// supplied its complete explicit authorization pair.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DatabaseMigrateArgs {
    pub config_path: Option<std::path::PathBuf>,
    pub initialization_reference: Option<String>,
    pub adoption_reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    AllInOne(ServiceArgs),
    Control(ServiceArgs),
    Coordinator(ServiceArgs),
    ControlIamProfile,
    ControlIamRuntimeProfile,
    ControlHostedRuntimeRouteProfile,
    DatabaseMigrate(DatabaseMigrateArgs),
    Config {
        json: bool,
        config_path: Option<std::path::PathBuf>,
    },
    Doctor {
        json: bool,
        config_path: Option<std::path::PathBuf>,
    },
    DoctorAcp {
        json: bool,
    },
    Version,
    Help,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceBinaryCommand {
    Serve(ServiceArgs),
    DatabaseMigrate(DatabaseMigrateArgs),
    Help,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
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
    if args == ["iam", "profile", "runtime"] {
        return Ok(Command::ControlIamRuntimeProfile);
    }
    if args == ["surface", "profile", "runtime"] {
        return Ok(Command::ControlHostedRuntimeRouteProfile);
    }
    if args.first().is_some_and(|arg| arg == "iam") {
        return Err("control iam requires `profile` or `profile runtime` exactly".to_owned());
    }
    if args.first().is_some_and(|arg| arg == "surface") {
        return Err("control surface requires `profile runtime` exactly".to_owned());
    }
    if args.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help);
    }
    parse_service_args(args).map(Command::Control)
}

fn parse_doctor_args(args: &[String]) -> Result<Command, String> {
    if args.first().is_some_and(|subject| subject == "acp") {
        let options = &args[1..];
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
        return Ok(Command::DoctorAcp { json });
    }
    parse_config_args(args).map(|command| match command {
        Command::Config { json, config_path } => Command::Doctor { json, config_path },
        Command::Help => Command::Help,
        _ => unreachable!("config parser returns only Config or Help"),
    })
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
    let mut initialize_installation = false;
    let mut initialization_reference = None;
    let mut adopt_unbound_existing = false;
    let mut adoption_reference = None;
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
            "--initialize-installation" if !initialize_installation => {
                initialize_installation = true;
            }
            "--initialize-installation" => {
                return Err("--initialize-installation may be supplied only once".into());
            }
            "--initialization-reference" if initialization_reference.is_none() => {
                index += 1;
                initialization_reference = Some(parse_exact_value(
                    args.get(index).map(String::as_str),
                    "--initialization-reference",
                )?);
            }
            value
                if value.starts_with("--initialization-reference=")
                    && initialization_reference.is_none() =>
            {
                initialization_reference = Some(parse_exact_value(
                    Some(&value[27..]),
                    "--initialization-reference",
                )?);
            }
            value if value.starts_with("--initialization-reference=") => {
                return Err(format!("duplicate database migrate argument {value:?}"));
            }
            "--adopt-unbound-existing" if !adopt_unbound_existing => {
                adopt_unbound_existing = true;
            }
            "--adopt-unbound-existing" => {
                return Err("--adopt-unbound-existing may be supplied only once".into());
            }
            "--adoption-reference" if adoption_reference.is_none() => {
                index += 1;
                adoption_reference = Some(parse_exact_value(
                    args.get(index).map(String::as_str),
                    "--adoption-reference",
                )?);
            }
            value if value.starts_with("--adoption-reference=") && adoption_reference.is_none() => {
                adoption_reference = Some(parse_exact_value(
                    Some(&value[21..]),
                    "--adoption-reference",
                )?);
            }
            value if value.starts_with("--adoption-reference=") => {
                return Err(format!("duplicate database migrate argument {value:?}"));
            }
            other => return Err(format!("unexpected database migrate argument {other:?}")),
        }
        index += 1;
    }
    let initialization_reference = complete_authorization_pair(
        initialize_installation,
        initialization_reference,
        "--initialize-installation",
        "--initialization-reference",
    )?;
    let adoption_reference = complete_authorization_pair(
        adopt_unbound_existing,
        adoption_reference,
        "--adopt-unbound-existing",
        "--adoption-reference",
    )?;
    Ok(Command::DatabaseMigrate(DatabaseMigrateArgs {
        config_path,
        initialization_reference,
        adoption_reference,
    }))
}

fn complete_authorization_pair(
    enabled: bool,
    reference: Option<String>,
    switch: &str,
    reference_flag: &str,
) -> Result<Option<String>, String> {
    match (enabled, reference) {
        (false, None) => Ok(None),
        (true, Some(reference)) => Ok(Some(reference)),
        (true, None) => Err(format!("{switch} requires {reference_flag} <REF>")),
        (false, Some(_)) => Err(format!("{reference_flag} requires {switch}")),
    }
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

fn parse_cloud_models(value: Option<&str>) -> Result<crate::config::CloudModelMode, String> {
    crate::config::CloudModelMode::parse(
        value.ok_or_else(|| "--cloud-models needs disabled or enabled".to_owned())?,
    )
}

fn parse_path(value: Option<&str>, flag: &str) -> Result<std::path::PathBuf, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a non-empty path"))
}

fn parse_exact_value(value: Option<&str>, flag: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty() && value.trim() == *value)
        .map(str::to_owned)
        .ok_or_else(|| format!("{flag} needs a non-empty value without surrounding whitespace"))
}

fn is_help(value: &str) -> bool {
    value == "-h" || value == "--help"
}

pub fn print_help() {
    println!(
        "Awaken\n\nUSAGE:\n    awaken [COMMAND] [OPTIONS]\n\nRunning `awaken` without a command is the same as `awaken all-in-one`.\n\nCOMMANDS:\n    all-in-one                      Run Control, Coordinator, and the local Worker together\n    control                         Run only the authoring and publication service\n    coordinator                     Run only Session, Run, Dispatch, and Worker coordination\n    control iam profile             Print the compiled Workspace IAM profile\n    control iam profile runtime     Print the compiled Hosted Runtime IAM profile\n    control surface profile runtime Print the hosted Control-to-Coordinator route profile\n    database migrate                Verify installation identity, apply deployment schema migrations, and exit\n    doctor [--json]                 Check configuration, storage, and listener readiness\n    doctor acp [--json]             Discover and diagnose supported local ACP agents\n    config [--json]                 Print effective, redacted configuration\n    version                         Print the installed version\n\nOPTIONS:\n    --config PATH                  Read typed configuration from PATH\n    --port PORT                    Override the listen port\n    --data-dir PATH                Override the persistent data root (default ~/.awaken)\n    --no-browser                   Do not open the browser\n    --identity-mode MODE           no-login, self-managed, or awaken-cloud\n    --cloud-models MODE            disabled or enabled (requires awaken-cloud identity)\n    --initialize-installation      Database-migrate-only first-install switch\n    --initialization-reference REF Required operator reference authorizing initialization\n    --adopt-unbound-existing       Database-migrate-only legacy adoption switch\n    --adoption-reference REF       Required operator reference authorizing legacy adoption\n    -h, --help                     Print this help\n\nThe execution service is the separate `awaken-worker` binary."
    );
}

/// Parse one role-specific executable. It serves by default and exposes only
/// that role's migration command; no role-selection command is accepted.
pub fn parse_service_binary_command(
    args: impl IntoIterator<Item = String>,
) -> Result<ServiceBinaryCommand, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.iter().any(|argument| is_help(argument)) {
        return Ok(ServiceBinaryCommand::Help);
    }
    if args.first().is_some_and(|argument| argument == "database") {
        return parse_database_args(&args[1..]).map(|command| match command {
            Command::DatabaseMigrate(args) => ServiceBinaryCommand::DatabaseMigrate(args),
            _ => unreachable!("database parser returns only migration"),
        });
    }
    parse_service_args(&args).map(ServiceBinaryCommand::Serve)
}

pub fn print_service_help(binary: &str) {
    println!(
        "{binary}\n\nUSAGE:\n    {binary} [OPTIONS]\n    {binary} database migrate [--config PATH] [INSTALLATION AUTHORIZATION]\n\nOPTIONS:\n    --config PATH                  Read typed configuration from PATH\n    --initialize-installation      Explicitly initialize local storage or a verified empty PostgreSQL target\n    --initialization-reference REF Required operator change/ticket reference for initialization\n    --adopt-unbound-existing       Explicitly adopt a verified legacy PostgreSQL target\n    --adoption-reference REF       Required operator change/ticket reference for legacy adoption\n    --port PORT                    Override the listen port\n    --data-dir PATH                Override the persistent data root\n    --identity-mode MODE           no-login, self-managed, or awaken-cloud\n    --cloud-models MODE            disabled or enabled\n    -h, --help                     Print this help"
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
        // command path; R5 service options -> stay attached to the selected role;
        // R6 bare/configured doctor -> deployment checks; R7 doctor acp -> the
        // existing runtime-specific diagnostic owner; invalid mixtures -> reject.
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
        assert!(
            parse_args([
                "control".into(),
                "iam".into(),
                "profile".into(),
                "resources".into()
            ])
            .is_err(),
            "the retired parallel resource profile has no compatibility command"
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
        assert_eq!(
            parse_args([
                "control".into(),
                "surface".into(),
                "profile".into(),
                "runtime".into()
            ])
            .unwrap(),
            Command::ControlHostedRuntimeRouteProfile
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
            Command::DatabaseMigrate(DatabaseMigrateArgs::default())
        );
        assert_eq!(
            parse_args([
                "database".into(),
                "migrate".into(),
                "--config".into(),
                "/etc/awaken/config.toml".into(),
            ])
            .unwrap(),
            Command::DatabaseMigrate(DatabaseMigrateArgs {
                config_path: Some("/etc/awaken/config.toml".into()),
                initialization_reference: None,
                adoption_reference: None,
            })
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
                cloud_models: Some(crate::config::CloudModelMode::Enabled),
                ..Default::default()
            })
        );
        for retired in ["start", "serve", "management", "worker"] {
            assert!(parse_args([retired.to_owned()]).is_err(), "R4 {retired}");
        }
        assert_eq!(
            parse_args(["doctor".into(), "acp".into(), "--json".into()]).unwrap(),
            Command::DoctorAcp { json: true }
        );
        assert_eq!(
            parse_args(["doctor".into()]).unwrap(),
            Command::Doctor {
                json: false,
                config_path: None,
            }
        );
        assert_eq!(
            parse_args([
                "doctor".into(),
                "--json".into(),
                "--config=/etc/awaken/config.toml".into(),
            ])
            .unwrap(),
            Command::Doctor {
                json: true,
                config_path: Some("/etc/awaken/config.toml".into()),
            }
        );
        assert!(parse_args(["doctor".into(), "providers".into()]).is_err());
    }

    #[test]
    fn role_binary_accepts_options_but_cannot_switch_role() {
        // Cause/effect decision table: B1 ordinary service options -> one parsed
        // option set; B2 help -> no service start; B3 a role/command token ->
        // reject. The executable supplies the role separately, so no argument
        // can make awaken-control acquire Coordinator authority or vice versa.
        assert_eq!(
            parse_service_binary_command([
                "--config".into(),
                "/etc/awaken/control.toml".into(),
                "--port=3000".into(),
            ])
            .unwrap(),
            ServiceBinaryCommand::Serve(ServiceArgs {
                config_path: Some("/etc/awaken/control.toml".into()),
                port: Some(3000),
                ..Default::default()
            }),
            "B1"
        );
        assert_eq!(
            parse_service_binary_command(["--help".into()]).unwrap(),
            ServiceBinaryCommand::Help,
            "B2"
        );
        assert!(
            parse_service_binary_command(["coordinator".into()]).is_err(),
            "B3"
        );
        assert_eq!(
            parse_service_binary_command([
                "database".into(),
                "migrate".into(),
                "--config=/etc/awaken/control.toml".into(),
            ])
            .unwrap(),
            ServiceBinaryCommand::DatabaseMigrate(DatabaseMigrateArgs {
                config_path: Some("/etc/awaken/control.toml".into()),
                initialization_reference: None,
                adoption_reference: None,
            }),
            "B4 migration remains inside the executable's fixed role"
        );
    }

    #[test]
    fn database_installation_authority_requires_complete_explicit_pairs() {
        /* Cause/effect graph: C1 initialization pair absent/complete/partial;
         * C2 adoption pair absent/complete/partial; C3 either value is blank or
         * duplicated. Effects: E1 ordinary migrate carries no bind authority;
         * E2 each complete pair carries its exact audit reference and both may
         * coexist for mixed physical targets; E3 every partial, ambiguous, or
         * blank request rejects before config/database access. Decision table:
         * A1 !C1+!C2=>E1; A2 complete C1+complete C2=>E2; A3 partial C1=>E3;
         * A4 partial C2=>E3; A5 blank/duplicate=>E3. */
        assert_eq!(
            parse_args(["database".into(), "migrate".into()]).unwrap(),
            Command::DatabaseMigrate(DatabaseMigrateArgs::default()),
            "A1"
        );
        assert_eq!(
            parse_args([
                "database".into(),
                "migrate".into(),
                "--initialize-installation".into(),
                "--initialization-reference=install-2048".into(),
                "--adopt-unbound-existing".into(),
                "--adoption-reference=change-2048".into(),
            ])
            .unwrap(),
            Command::DatabaseMigrate(DatabaseMigrateArgs {
                config_path: None,
                initialization_reference: Some("install-2048".into()),
                adoption_reference: Some("change-2048".into()),
            }),
            "A2"
        );
        for (rule, args) in [
            (
                "A3",
                vec![
                    "database".into(),
                    "migrate".into(),
                    "--initialize-installation".into(),
                ],
            ),
            (
                "A4",
                vec![
                    "database".into(),
                    "migrate".into(),
                    "--initialization-reference=install-2048".into(),
                ],
            ),
            (
                "A5",
                vec![
                    "database".into(),
                    "migrate".into(),
                    "--adopt-unbound-existing".into(),
                    "--adoption-reference= ".into(),
                ],
            ),
            (
                "A4 adoption",
                vec![
                    "database".into(),
                    "migrate".into(),
                    "--adoption-reference=change-2048".into(),
                ],
            ),
        ] {
            assert!(parse_args(args).is_err(), "{rule}");
        }
    }
}
