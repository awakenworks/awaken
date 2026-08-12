//! File and URL mechanics used while resolving deployment configuration.

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use awaken_runtime_host::DispatchBackend;

use super::{OperatingMode, Role};

pub(super) fn validate_suite_hub_url(value: &str, mode: OperatingMode) -> Result<String, String> {
    if value.trim() != value || value.is_empty() {
        return Err("suite_hub_url must be a non-empty exact URL".to_owned());
    }
    let parsed = url::Url::parse(value).map_err(|_| "suite_hub_url must be an absolute URL")?;
    let loopback_http = mode == OperatingMode::Local
        && parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if parsed.scheme() != "https" && !loopback_http {
        return Err("suite_hub_url must use HTTPS (or loopback HTTP in local mode)".to_owned());
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("suite_hub_url must not contain credentials, query, or fragment".to_owned());
    }
    Ok(value.to_owned())
}

pub(super) fn read_database_url_file(path: &Path, field: &str) -> Result<String, String> {
    let value = fs::read_to_string(path)
        .map_err(|error| format!("read {field} {}: {error}", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} {} is empty", path.display()));
    }
    if !is_postgres_url(value) {
        return Err(format!("{field} must contain a postgres:// URL"));
    }
    Ok(value.to_owned())
}

pub(super) fn resolve_runtime_database_url(
    inline: Option<&String>,
    file: Option<&Path>,
) -> Result<Option<String>, String> {
    if inline.is_some() && file.is_some() {
        return Err(
            "runtime_database_url and runtime_database_url_file are mutually exclusive".into(),
        );
    }
    file.map(|path| read_database_url_file(path, "runtime_database_url_file"))
        .transpose()
        .map(|from_file| from_file.or_else(|| inline.cloned()))
}

pub(super) fn select_store_url(
    specific: Option<&String>,
    coordinator_runtime: Option<&String>,
    shared_management: Option<&String>,
) -> Option<String> {
    specific
        .cloned()
        .or_else(|| coordinator_runtime.cloned())
        .or_else(|| shared_management.cloned())
}

pub(super) fn resolve_dispatch_backend(
    database_url: Option<&String>,
    role: Role,
    run_local_pool: bool,
) -> Result<DispatchBackend, String> {
    if database_url.is_some_and(|url| !is_postgres_url(url)) {
        return Err("runtime_database_url must be postgres://".to_owned());
    }
    let backend = if database_url.is_some() {
        DispatchBackend::Postgres
    } else {
        DispatchBackend::Sqlite
    };
    if role == Role::Coordinator && backend != DispatchBackend::Postgres {
        return Err("Coordinator requires runtime_database_url".to_owned());
    }
    if role == Role::AllInOne && !run_local_pool && backend != DispatchBackend::Postgres {
        return Err("run_local_pool=false requires runtime_database_url".to_owned());
    }
    Ok(backend)
}

pub(super) fn override_port(bind: &str, port: u16) -> Result<String, String> {
    let mut address = bind
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("invalid bind address {bind:?}; expected IP:PORT"))?;
    address.set_port(port);
    Ok(address.to_string())
}

pub(super) fn is_postgres_url(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.starts_with("postgres://") || lower.starts_with("postgresql://")
}

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

pub(super) fn read_or_create_local_key(path: &Path) -> Result<String, String> {
    if path.exists() {
        return fs::read_to_string(path)
            .map_err(|error| format!("read local seal key {}: {error}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let encoded = awaken_credential_store::generate_seal_key_hex();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(encoded.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("write local seal key {}: {error}", path.display()))?;
            Ok(encoded)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut value = String::new();
            fs::File::open(path)
                .and_then(|mut file| file.read_to_string(&mut value))
                .map_err(|error| format!("read local seal key {}: {error}", path.display()))?;
            Ok(value)
        }
        Err(error) => Err(format!("create local seal key {}: {error}", path.display())),
    }
}
