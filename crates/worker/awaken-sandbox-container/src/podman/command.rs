//! Canonical rootless Podman child-process environment.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::FileTypeExt as _;
use std::path::Path;

use tokio::process::Command as OsCommand;

/// Resolve the canonical rootless systemd user-manager bus.
/// Podman uses this bus to create a delegated cgroup scope; without it, resource
/// limits fail even though the user's systemd manager and cgroup delegation are
/// healthy. Desktop sessions may expose another live D-Bus socket that does not
/// own `org.freedesktop.systemd1`; the XDG runtime bus is therefore the sole
/// authority for this Podman subprocess. A missing/non-socket endpoint is not
/// papered over: Podman remains the authority for the fail-closed diagnostic.
pub(super) fn rootless_systemd_bus(runtime_dir: Option<&OsStr>) -> Option<OsString> {
    let bus = Path::new(runtime_dir?).join("bus");
    let metadata = std::fs::symlink_metadata(&bus).ok()?;
    if !metadata.file_type().is_socket() {
        return None;
    }
    Some(format!("unix:path={}", bus.to_str()?).into())
}

/// Sole production constructor for Podman child processes. Keeping the
/// rootless-session adaptation here prevents run/exec/signal from drifting into
/// three subtly different host-environment contracts.
pub(super) fn podman_command(bin: &str) -> OsCommand {
    let mut command = OsCommand::new(bin);
    command.kill_on_drop(true);
    if let Some(address) = rootless_systemd_bus(std::env::var_os("XDG_RUNTIME_DIR").as_deref()) {
        command.env("DBUS_SESSION_BUS_ADDRESS", address);
    }
    command
}
