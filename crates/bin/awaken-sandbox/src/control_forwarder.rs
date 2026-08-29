//! Pod-local payload-opaque forwarder for one demanded Sandbox control service.

use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, copy_bidirectional};
#[cfg(target_os = "linux")]
use tokio::net::{TcpListener, UnixListener};
#[cfg(target_os = "linux")]
use tokio::time::timeout;

#[cfg(target_os = "linux")]
const CONTROL_EXCHANGE_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ControlForwarderError {
    #[error("invalid control-forwarder invocation")]
    InvalidInvocation,
    #[error("Sandbox control forwarder is unavailable")]
    Unavailable,
}

fn parse_args(args: &[String]) -> Result<(PathBuf, SocketAddr, PathBuf), ControlForwarderError> {
    let [unix_flag, unix, listen_flag, listen, ready_flag, ready] = args else {
        return Err(ControlForwarderError::InvalidInvocation);
    };
    if unix_flag != "--unix"
        || unix != awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH
        || listen_flag != "--listen"
        || ready_flag != "--ready"
        || ready != awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH
    {
        return Err(ControlForwarderError::InvalidInvocation);
    }
    let listen = listen
        .parse::<SocketAddr>()
        .map_err(|_| ControlForwarderError::InvalidInvocation)?;
    if !listen.ip().is_loopback() || listen.port() == 0 {
        return Err(ControlForwarderError::InvalidInvocation);
    }
    Ok((PathBuf::from(unix), listen, PathBuf::from(ready)))
}

fn parse_ready_args(args: &[String]) -> Result<PathBuf, ControlForwarderError> {
    let [marker_flag, marker] = args else {
        return Err(ControlForwarderError::InvalidInvocation);
    };
    if marker_flag != "--marker"
        || marker != awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH
    {
        return Err(ControlForwarderError::InvalidInvocation);
    }
    Ok(PathBuf::from(marker))
}

#[cfg(target_os = "linux")]
fn prepare_socket(path: &Path) -> Result<(), ControlForwarderError> {
    let parent = path
        .parent()
        .ok_or(ControlForwarderError::InvalidInvocation)?;
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|_| ControlForwarderError::Unavailable)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ControlForwarderError::Unavailable);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|_| ControlForwarderError::Unavailable)?;
        }
        Ok(_) => return Err(ControlForwarderError::Unavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ControlForwarderError::Unavailable),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
struct MarkerIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn marker_identity(path: &Path) -> Result<MarkerIdentity, ControlForwarderError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| ControlForwarderError::Unavailable)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ControlForwarderError::Unavailable);
    }
    Ok(MarkerIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
fn unlink_marker_if_identity(path: &Path, identity: MarkerIdentity) {
    if marker_identity(path).ok() == Some(identity) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(target_os = "linux")]
fn remove_stale_marker(path: &Path) -> Result<(), ControlForwarderError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            std::fs::remove_file(path).map_err(|_| ControlForwarderError::Unavailable)
        }
        Ok(_) => Err(ControlForwarderError::Unavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ControlForwarderError::Unavailable),
    }
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Result<String, ControlForwarderError> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|_| ControlForwarderError::Unavailable)?;
    let command_end = stat.rfind(')').ok_or(ControlForwarderError::Unavailable)?;
    let start_time = stat[command_end + 1..]
        .split_whitespace()
        .nth(19)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or(ControlForwarderError::Unavailable)?;
    Ok(start_time.to_owned())
}

#[cfg(target_os = "linux")]
fn marker_payload(pid: u32) -> Result<String, ControlForwarderError> {
    Ok(format!("v1 {pid} {}\n", process_start_time(pid)?))
}

#[cfg(target_os = "linux")]
struct ReadyMarker {
    path: PathBuf,
    identity: MarkerIdentity,
    _file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Drop for ReadyMarker {
    fn drop(&mut self) {
        unlink_marker_if_identity(&self.path, self.identity);
    }
}

#[cfg(target_os = "linux")]
fn publish_ready_marker(path: &Path) -> Result<ReadyMarker, ControlForwarderError> {
    use std::io::Write as _;

    remove_stale_marker(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ControlForwarderError::InvalidInvocation)?;
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    remove_stale_marker(&temporary)?;
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| ControlForwarderError::Unavailable)?;
        file.write_all(marker_payload(std::process::id())?.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| ControlForwarderError::Unavailable)?;
        std::fs::rename(&temporary, path).map_err(|_| ControlForwarderError::Unavailable)?;
        let identity = marker_identity(path)?;
        Ok(ReadyMarker {
            path: path.to_path_buf(),
            identity,
            _file: file,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(target_os = "linux")]
fn ready_marker_is_current(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 128
    {
        return false;
    }
    let Ok(payload) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut fields = payload.split_whitespace();
    let (Some("v1"), Some(pid), Some(expected_start), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    pid != 0
        && expected_start.bytes().all(|byte| byte.is_ascii_digit())
        && process_start_time(pid).is_ok_and(|actual| actual == expected_start)
        && std::fs::metadata(format!("/proc/{pid}"))
            .is_ok_and(|process| process.uid() == metadata.uid())
}

pub fn check_ready(args: &[String]) -> Result<(), ControlForwarderError> {
    let marker = parse_ready_args(args)?;
    #[cfg(target_os = "linux")]
    if ready_marker_is_current(&marker) {
        return Ok(());
    }
    let _ = marker;
    Err(ControlForwarderError::Unavailable)
}

#[cfg(target_os = "linux")]
pub async fn run(args: &[String]) -> Result<(), ControlForwarderError> {
    let (unix_path, listen, ready_path) = parse_args(args)?;
    remove_stale_marker(&ready_path)?;
    prepare_socket(&unix_path)?;
    let unix = UnixListener::bind(&unix_path).map_err(|_| ControlForwarderError::Unavailable)?;
    std::fs::set_permissions(&unix_path, std::fs::Permissions::from_mode(0o777))
        .map_err(|_| ControlForwarderError::Unavailable)?;
    let tcp = TcpListener::bind(listen)
        .await
        .map_err(|_| ControlForwarderError::Unavailable)?;
    let _ready = publish_ready_marker(&ready_path)?;

    // Fixed concurrency=1 matches the host publisher's one-request channels.
    // An established provider channel may be idle for the whole Session, so
    // neither accept nor the wait for the helper's first byte has an exchange
    // deadline. Once the request starts, the remaining byte exchange is bounded.
    loop {
        let (mut remote, _) = tcp
            .accept()
            .await
            .map_err(|_| ControlForwarderError::Unavailable)?;
        let (mut local, _) = unix
            .accept()
            .await
            .map_err(|_| ControlForwarderError::Unavailable)?;
        let mut first_byte = [0_u8; 1];
        if local.read_exact(&mut first_byte).await.is_err() {
            continue;
        }
        let _ = timeout(CONTROL_EXCHANGE_DEADLINE, async {
            remote.write_all(&first_byte).await?;
            copy_bidirectional(&mut remote, &mut local).await
        })
        .await;
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn run(args: &[String]) -> Result<(), ControlForwarderError> {
    let _ = parse_args(args)?;
    Err(ControlForwarderError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarder_accepts_only_exact_private_socket_and_loopback() {
        /* R14 cause/effect decision table: exact runtime-owned socket plus
         * loopback address => the sole provider channel; workspace path,
         * wildcard address, or alternate role arguments => reject before bind.
         */
        let socket = awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH;
        assert!(
            parse_args(&[
                "--unix".into(),
                socket.into(),
                "--listen".into(),
                "127.0.0.1:7778".into(),
                "--ready".into(),
                awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.into(),
            ])
            .is_ok(),
            "R14 exact private channel",
        );
        assert!(
            parse_args(&[
                "--unix".into(),
                "/workspace/.awaken/control.sock".into(),
                "--listen".into(),
                "127.0.0.1:7778".into(),
                "--ready".into(),
                awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.into(),
            ])
            .is_err(),
            "R14 workspace rendezvous denied",
        );
        assert!(
            parse_args(&[
                "--unix".into(),
                socket.into(),
                "--listen".into(),
                "0.0.0.0:7778".into(),
                "--ready".into(),
                awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.into(),
            ])
            .is_err(),
            "R14 non-loopback denied",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn readiness_marker_is_atomic_current_and_restart_fenced() {
        /* Marker cause/effect table: C1=current forwarder PID/starttime and
         * mode-0600 regular inode; C2=stale starttime/PID; C3=guard exits;
         * C4=another inode replaces the marker. E1=readiness succeeds without
         * touching the business port; E2=stale evidence fails; E3=owned inode
         * is removed; E4=replacement survives. Rules M1 C1=>E1;
         * M2 C2=>E2; M3 C1+C3=>E3; M4 C3+C4=>E4.
         */
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ready");
        let ready = publish_ready_marker(&marker).unwrap();
        assert!(ready_marker_is_current(&marker), "M1/E1");
        std::fs::write(&marker, format!("v1 {} 0\n", std::process::id())).unwrap();
        assert!(!ready_marker_is_current(&marker), "M2/E2");
        drop(ready);
        assert!(!marker.exists(), "M3/E3");

        let ready = publish_ready_marker(&marker).unwrap();
        std::fs::remove_file(&marker).unwrap();
        std::fs::write(&marker, marker_payload(std::process::id()).unwrap()).unwrap();
        drop(ready);
        assert!(marker.exists(), "M4/E4 replacement inode");
    }
}
