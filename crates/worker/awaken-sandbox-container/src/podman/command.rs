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
    // A Session container outlives its Worker service. Podman interprets the
    // parent's INVOCATION_ID as permission to retain conmon in that service's
    // cgroup; systemd then kills it on Worker exit. Network helpers also need
    // the activation scope below: moving conmon alone does not move its siblings.
    // Let Podman's existing cgroup manager own the independent container.
    // Only this child environment changes; the Worker keeps its manager identity.
    command.env_remove("INVOCATION_ID");
    if let Some(address) = rootless_systemd_bus(std::env::var_os("XDG_RUNTIME_DIR").as_deref()) {
        command.env("DBUS_SESSION_BUS_ADDRESS", address);
    }
    command
}

/// Only activation can create container-lifetime network/port helpers. They must
/// share a Podman-owned scope, outside a restarting rootless Worker service.
/// `--scope` preserves the caller's cwd/env/stdio and returns the CLI result;
/// systemd collects the scope after Podman's last helper exits. There is no
/// application supervisor, remote daemon, cgroup migration or network rewrite.
/// Readiness/exec/stop/rm stay call-owned. An unavailable user manager fails
/// activation; it must not fall back to a service-bound container.
pub(super) fn podman_subcommand(bin: &str, args: &[String]) -> OsCommand {
    let activation = matches!(args.first().map(String::as_str), Some("run" | "start"));
    let managed = std::env::var_os("INVOCATION_ID").is_some_and(|id| !id.is_empty());
    let mut command = if activation && managed {
        let mut command = podman_command("systemd-run");
        command.args(["--user", "--scope", "--quiet", "--collect", "--", bin]);
        command
    } else {
        podman_command(bin)
    };
    command.args(args);
    command
}
