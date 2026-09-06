//! Verified external Linux owner. No unit installation, ambient opt-in flag,
//! PID registry, or fallback process-tree claim lives here.
//!
//! TrustedHost workloads must not deliberately migrate out of the delegated
//! hierarchy or reconfigure their supervisor. This is crash containment, not
//! confinement against an adversarial process running as the host user.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use awaken_provisioning_contract::ProcessHandle as _;
use tokio::io::AsyncReadExt;

use crate::LocalProcess;
use crate::cgroup::{CgroupIdentity, LinuxProcessScope};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_OBSERVATION: u64 = 16 * 1024;

/// An observed service incarnation, never constructed from a user assertion.
/// Scope creation revalidates it. A production launch owner must also call
/// revalidate before admission; this adapter is not yet installed in Worker.
/// An observation cannot authorize an effect after a health change.
pub struct LinuxProcessSupervisor {
    manager: Manager,
    unit: String,
    invocation: String,
    boot_id: String,
    path: PathBuf,
    identity: CgroupIdentity,
}

#[derive(Clone, Copy)]
enum Manager {
    User,
    System,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn membership() -> io::Result<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup")?;
    let paths: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .collect();
    let [path] = paths.as_slice() else {
        return Err(invalid("exact unified cgroup membership is unavailable"));
    };
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        || path == Path::new("/")
    {
        return Err(invalid("invalid delegated cgroup membership"));
    }
    Ok(Path::new(CGROUP_ROOT).join(
        path.strip_prefix("/")
            .map_err(|_| invalid("invalid cgroup root"))?,
    ))
}

pub fn host_boot_id() -> io::Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(invalid("invalid host boot identity"));
    }
    Ok(value.to_owned())
}

async fn observe(manager: Manager, unit: &str) -> io::Result<BTreeMap<String, String>> {
    // Fixed system executable; never resolve a workload-projected PATH or pass
    // arbitrary command fragments to a shell. Only exact unit read operations.
    let mut command = tokio::process::Command::new("/usr/bin/systemctl");
    if matches!(manager, Manager::User) {
        command.arg("--user");
    }
    command.args([
        "show", "--no-pager",
        "--property=MainPID,ControlGroup,Delegate,KillMode,SendSIGKILL,FinalKillSignal,RemainAfterExit,ActiveState,SubState,InvocationID,Type,ExitType,NotifyAccess,TimeoutStopUSec,TimeoutStopFailureMode",
        "--", unit,
    ]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut process = LocalProcess::spawn(&mut command)?;
    let mut output = process
        .take_stdout()
        .ok_or_else(|| invalid("supervisor probe has no output"))?;
    let observation = tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut bytes = Vec::new();
        (&mut output)
            .take(MAX_OBSERVATION + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_OBSERVATION {
            return Err(invalid("supervisor observation exceeded its bound"));
        }
        if process
            .wait()
            .await
            .map_err(|_| invalid("supervisor probe wait failed"))?
            .code
            != Some(0)
        {
            return Err(invalid("supervisor unit observation failed"));
        }
        parse_properties(
            std::str::from_utf8(&bytes).map_err(|_| invalid("invalid supervisor observation"))?,
        )
    })
    .await;
    match observation {
        Ok(result) => result,
        Err(_) => {
            let _ = process
                .signal(awaken_provisioning_contract::Signal::Kill)
                .await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "supervisor observation timed out",
            ))
        }
    }
}

fn parse_properties(text: &str) -> io::Result<BTreeMap<String, String>> {
    let mut properties = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid("invalid unit property"))?;
        if properties
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(invalid("duplicate unit property"));
        }
    }
    Ok(properties)
}

fn validate(properties: &BTreeMap<String, String>, pid: u32, group: &str) -> io::Result<String> {
    for (key, expected) in [
        ("MainPID", pid.to_string()),
        ("ControlGroup", group.to_owned()),
        ("Delegate", "yes".to_owned()),
        ("KillMode", "control-group".to_owned()),
        ("SendSIGKILL", "yes".to_owned()),
        ("FinalKillSignal", "9".to_owned()),
        ("ExitType", "main".to_owned()),
        ("NotifyAccess", "none".to_owned()),
        ("TimeoutStopFailureMode", "terminate".to_owned()),
        ("RemainAfterExit", "no".to_owned()),
        ("ActiveState", "active".to_owned()),
        ("SubState", "running".to_owned()),
    ] {
        if properties.get(key) != Some(&expected) {
            return Err(invalid(
                "Worker is not the main process of an eligible delegated service",
            ));
        }
    }
    if !matches!(
        properties.get("Type").map(String::as_str),
        Some("exec" | "simple")
    ) || !properties
        .get("TimeoutStopUSec")
        .is_some_and(|value| bounded_stop_delay(value))
    {
        return Err(invalid(
            "external supervisor requires a non-extendable stop deadline within 30 seconds",
        ));
    }
    let invocation = properties
        .get("InvocationID")
        .ok_or_else(|| invalid("missing service incarnation"))?;
    if invocation.len() != 32 || !invocation.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid service incarnation"));
    }
    Ok(invocation.clone())
}

fn bounded_stop_delay(value: &str) -> bool {
    // systemctl formats finite microsecond durations as integer unit components.
    // Accept only the bounded subset needed by the Worker service contract,
    // never infinity, zero/disabled, unknown units or overflowing observations.
    let total = value.split_whitespace().try_fold(0_u64, |total, part| {
        let split = part.find(|character: char| !character.is_ascii_digit())?;
        let number = part[..split].parse::<u64>().ok()?;
        let unit = match &part[split..] {
            "us" | "µs" => 1,
            "ms" => 1_000,
            "s" => 1_000_000,
            "min" => 60_000_000,
            _ => return None,
        };
        total.checked_add(number.checked_mul(unit)?)
    });
    total.is_some_and(|value| (1..=30_000_000).contains(&value))
}

impl LinuxProcessSupervisor {
    /// Discover only this process's exact service. Missing delegation/systemd
    /// is an unavailable capability, never an automatic system reconfiguration.
    pub async fn detect() -> io::Result<Self> {
        let path = membership()?;
        let unit = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.ends_with(".service") && !name.starts_with('-'))
            .ok_or_else(|| invalid("Worker must be the service main process"))?
            .to_owned();
        let group = format!(
            "/{}",
            path.strip_prefix(CGROUP_ROOT)
                .map_err(|_| invalid("invalid control group"))?
                .display()
        );
        let scope = LinuxProcessScope::open(&path)?;
        for manager in [Manager::User, Manager::System] {
            if let Ok(properties) = observe(manager, &unit).await
                && let Ok(invocation) = validate(&properties, std::process::id(), &group)
            {
                let supervisor = Self {
                    manager,
                    unit: unit.clone(),
                    invocation,
                    boot_id: host_boot_id()?,
                    path: path.clone(),
                    identity: scope.identity(),
                };
                supervisor.revalidate().await?;
                return Ok(supervisor);
            }
        }
        Err(invalid("no eligible external process-tree supervisor"))
    }

    pub async fn revalidate(&self) -> io::Result<()> {
        if membership()? != self.path
            || host_boot_id()? != self.boot_id
            || LinuxProcessScope::open(&self.path)?.identity() != self.identity
        {
            return Err(invalid("external supervisor physical identity changed"));
        }
        let group = format!(
            "/{}",
            self.path
                .strip_prefix(CGROUP_ROOT)
                .map_err(|_| invalid("invalid control group"))?
                .display()
        );
        let invocation = validate(
            &observe(self.manager, &self.unit).await?,
            std::process::id(),
            &group,
        )?;
        if invocation != self.invocation {
            return Err(invalid("external supervisor service incarnation changed"));
        }
        Ok(())
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Allocate one fresh leaf. Caller owns the realization lock and must
    /// persist this exact identity before authorizing any workload launch.
    /// Existing names (including symlinks) are never adopted as new scopes.
    pub async fn create_scope(&self, nonce: &str) -> io::Result<LinuxProcessScope> {
        if nonce.len() != 64 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("process scope needs a fresh physical incarnation"));
        }
        self.revalidate().await?;
        let parent = rustix::fs::open(
            &self.path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let identity = rustix::fs::fstat(&parent)?;
        if identity.st_dev != self.identity.device || identity.st_ino != self.identity.inode {
            return Err(invalid("supervisor changed before scope creation"));
        }
        let leaf = format!("awaken-session-{nonce}");
        rustix::fs::mkdirat(&parent, leaf.as_str(), rustix::fs::Mode::RWXU)?;
        let scope = LinuxProcessScope::open(&self.path.join(leaf))?;
        self.revalidate().await?;
        Ok(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible() -> BTreeMap<String, String> {
        parse_properties("MainPID=123\nControlGroup=/user.slice/worker.service\nDelegate=yes\nKillMode=control-group\nSendSIGKILL=yes\nFinalKillSignal=9\nExitType=main\nNotifyAccess=none\nTimeoutStopFailureMode=terminate\nType=exec\nTimeoutStopUSec=1s\nRemainAfterExit=no\nActiveState=active\nSubState=running\nInvocationID=0123456789abcdef0123456789abcdef\n").unwrap()
    }

    #[test]
    fn external_owner_decision_table_rejects_every_missing_or_changed_prerequisite() {
        // SUP1 exact main PID + group + delegation + whole-group KILL + final
        // SIGKILL + non-resident active/running service + incarnation => accept.
        // SUP2 any one missing/changed prerequisite => reject. These causes are
        // conjunctive, so single-fault rows cover each independent veto; no
        // subset can compensate for another missing prerequisite.
        let good = eligible();
        assert!(validate(&good, 123, "/user.slice/worker.service").is_ok());
        for key in good.keys() {
            let mut missing = good.clone();
            missing.remove(key);
            assert!(
                validate(&missing, 123, "/user.slice/worker.service").is_err(),
                "missing {key}"
            );
            let mut wrong = good.clone();
            wrong.insert(key.clone(), "wrong".to_owned());
            assert!(
                validate(&wrong, 123, "/user.slice/worker.service").is_err(),
                "changed {key}"
            );
        }
        // SUP3 ambiguous/malformed output cannot be accepted by last-write-wins.
        assert!(parse_properties("MainPID=123\nMainPID=123\n").is_err());
        assert!(parse_properties("MainPID\n").is_err());
    }

    #[test]
    fn stop_deadline_and_service_lifetime_cannot_hide_surviving_children() {
        // SUP8 boundary/equivalence rows: finite positive stop <=30s accepts;
        // infinity/zero/overflow/unsupported format/over-budget rejects.
        // SUP9 ExitType=cgroup, notify extensions or a catchable final signal
        // cannot supply main-process-crash containment. SUP2 independently
        // checks every field's absence and corruption.
        for (text, accepted) in [
            ("1us", true),
            ("1µs", true),
            ("500ms", true),
            ("30s", true),
            ("1s 500ms", true),
            ("30s 1us", false),
            ("1min", false),
            ("0", false),
            ("0s", false),
            ("infinity", false),
            ("", false),
            ("18446744073709551615min", false),
            ("1unknown", false),
        ] {
            assert_eq!(bounded_stop_delay(text), accepted, "{text}");
        }
        let mut simple = eligible();
        simple.insert("Type".into(), "simple".into());
        assert!(validate(&simple, 123, "/user.slice/worker.service").is_ok());
        for (key, value) in [
            ("ExitType", "cgroup"),
            ("NotifyAccess", "all"),
            ("FinalKillSignal", "15"),
            ("TimeoutStopUSec", "infinity"),
            ("TimeoutStopFailureMode", "abort"),
            ("Type", "notify"),
        ] {
            let mut invalid = eligible();
            invalid.insert(key.into(), value.into());
            assert!(validate(&invalid, 123, "/user.slice/worker.service").is_err());
        }
    }
}
