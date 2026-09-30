//! The Workdir tier's persistent shell: which Worker variables it inherits and,
//! when tool egress is denied, the bubblewrap launch that confines it.

/// Worker variables a Workdir shell inherits. The Worker environment carries
/// operator secrets (provider keys, database URLs, seal keys, cloud and Git
/// tokens) under names no deny-list can enumerate, and the shell runs
/// model-authored commands, so inheritance is an allow-list of variables that
/// only locate tools, select locale/terminal behaviour, or route through the
/// operator's proxy and CA trust. Everything else is withheld.
const WORKDIR_BASH_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "GOBIN",
    "JAVA_HOME",
    "NVM_DIR",
    "NVM_BIN",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_ADDRESS",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_NAME",
    "LC_PAPER",
    "LC_TELEPHONE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Windows process startup locators. They contain installation paths, not
/// credentials, and both opaque Workdir children and its Bash session need
/// them after their inherited environment is cleared.
#[cfg(windows)]
pub(crate) const WINDOWS_OS_LOCATORS: &[&str] = &["SystemRoot", "WINDIR"];

pub(crate) fn workdir_bash_passthrough(
    worker_env: impl IntoIterator<Item = (String, String)>,
) -> std::collections::BTreeMap<String, String> {
    worker_env
        .into_iter()
        .filter(|(name, value)| {
            (WORKDIR_BASH_PASSTHROUGH.contains(&name.as_str()) || {
                #[cfg(windows)]
                {
                    WINDOWS_OS_LOCATORS
                        .iter()
                        .any(|key| key.eq_ignore_ascii_case(name))
                }
                #[cfg(not(windows))]
                {
                    false
                }
            }) && (!matches!(
                name.as_str(),
                "http_proxy" | "https_proxy" | "HTTP_PROXY" | "HTTPS_PROXY"
            ) || url::Url::parse(value).is_ok_and(|proxy| {
                matches!(proxy.scheme(), "http" | "https")
                    && proxy.host_str().is_some()
                    && proxy.username().is_empty()
                    && proxy.password().is_none()
            }))
        })
        .collect()
}

/// Build the trusted launcher argv for one persistent Bash process in a
/// networkless bubblewrap namespace rooted at the environment workdir.
///
/// The Workdir tier is a trusted-host tier: the shell keeps a read-only view of
/// the host filesystem. What `deny_egress` promises is *no egress*, and a host
/// view is also a view of every daemon socket on the host (container engine,
/// system bus, resolver) — each one a way out that `--unshare-net` does not
/// close. So the host `/run` is masked, the launch denies creating `AF_UNIX`
/// sockets (see [`workdir_bash_launch_preparation`]), and the shell lives in
/// its own user/PID namespace and session, dying with its launcher.
pub(crate) fn bwrap_persistent_bash(root: &str) -> Vec<String> {
    let mut argv: Vec<String> = awaken_local_process::BUBBLEWRAP_PROCESS_BOUNDARY
        .into_iter()
        .map(str::to_owned)
        .collect();
    argv.extend(awaken_local_process::bubblewrap_seccomp_args());
    argv.extend(
        [
            "--unshare-net",
            "--unshare-ipc",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--bind",
            root,
            root,
            "--chdir",
            root,
            "--",
            "/bin/bash",
            "--noprofile",
            "--norc",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    argv
}

/// Hand the networkless Workdir shell's launcher its syscall profile: every
/// start maps fresh sealed descriptors, so no state crosses shell restarts.
pub(crate) fn workdir_bash_launch_preparation() -> awaken_ext_builtin_tools::BashLaunchPreparation {
    std::sync::Arc::new(|command: &mut tokio::process::Command| {
        #[cfg(target_os = "linux")]
        {
            crate::namespace::attach_workload_syscall_profile(
                command,
                awaken_local_process::seccomp::SyscallProfile::NoUnixSockets,
                Vec::new(),
            )
            .map_err(std::io::Error::other)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Err(std::io::Error::other(
                "a networkless Workdir shell requires bubblewrap on Linux",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inheritance decision table. Causes: C1 the name is on the fixed
    /// allow-list (including exact locale/XDG names); C2 it is a proxy locator
    /// without credentials; C3 it is unknown or carries credentials. Effects:
    /// C1 and applicable C2 => inherited verbatim; C3 => withheld. Unknown
    /// prefixed names stay unknown, so LC_/XDG_ cannot carry an unseen secret.
    #[test]
    fn workdir_shell_inherits_only_locator_and_locale_variables() {
        let worker = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/op"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("CARGO_HOME", "/home/op/.cargo"),
            ("LC_ALL", "C.UTF-8"),
            ("XDG_CACHE_HOME", "/home/op/.cache"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("OPENAI_API_KEY", "sk-openai"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("GITHUB_TOKEN", "ghp"),
            ("DATABASE_URL", "postgres://u:p@db/x"),
            ("AWAKEN_MGMT_SEAL_KEY", "seal"),
            ("LD_PRELOAD", "/tmp/hook.so"),
            ("SOME_FUTURE_SECRET", "x"),
            ("LC_SECRET_TOKEN", "x"),
            ("XDG_API_KEY", "x"),
            ("http_proxy", "http://user:password@proxy:3128"),
            ("https_proxy", "https://token@proxy:3128"),
            ("HTTP_PROXY", "not a proxy URL"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let inherited = workdir_bash_passthrough(worker);
        assert_eq!(
            inherited.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "CARGO_HOME",
                "HOME",
                "HTTPS_PROXY",
                "LC_ALL",
                "PATH",
                "XDG_CACHE_HOME"
            ]
        );
        assert_eq!(inherited["PATH"], "/usr/bin");
    }

    #[cfg(windows)]
    #[test]
    fn windows_workdir_shell_inherits_os_startup_locators_only() {
        // C1 Windows OS startup locator or arbitrary Worker secret; E1 the
        // former reaches a cleared child env unchanged; E2 the latter never
        // enters the model-authored persistent shell.
        let inherited = workdir_bash_passthrough([
            ("SystemRoot".into(), r"C:\Windows".into()),
            ("WINDIR".into(), r"C:\Windows".into()),
            ("AWAKEN_SECRET".into(), "hidden".into()),
        ]);
        assert_eq!(inherited.len(), 2, "E1/E2");
        assert_eq!(inherited["SystemRoot"], r"C:\Windows", "E1");
        assert_eq!(inherited["WINDIR"], r"C:\Windows", "E1");
    }

    /// Networkless shell launch profile: the process boundary, the syscall
    /// profile options, no network, a masked host `/run` and `/tmp`, the
    /// workdir as the only writable bind, and the fixed shell after `--`.
    #[test]
    fn networkless_shell_launch_confines_process_network_and_host_sockets() {
        // The root is an argv token, never interpolated into shell text.
        let argv = bwrap_persistent_bash("/srv/wo'rk");
        let options = &argv[..argv.iter().position(|arg| arg == "--").unwrap()];
        assert!(options.iter().any(|flag| flag == "--disable-userns"));
        for flag in awaken_local_process::BUBBLEWRAP_PROCESS_BOUNDARY
            .into_iter()
            .chain(["--unshare-net", "--unshare-ipc"])
        {
            assert_eq!(
                options.iter().filter(|arg| *arg == flag).count(),
                1,
                "{flag}"
            );
        }
        let seccomp = awaken_local_process::bubblewrap_seccomp_args();
        assert!(
            options
                .windows(seccomp.len())
                .any(|window| window == seccomp)
        );
        for masked in ["/tmp", "/run"] {
            assert!(
                options.windows(2).any(|w| w == ["--tmpfs", masked]),
                "{masked}"
            );
        }
        let writable: Vec<_> = options
            .windows(3)
            .filter(|w| w[0] == "--bind")
            .map(|w| (w[1].as_str(), w[2].as_str()))
            .collect();
        assert_eq!(writable, [("/srv/wo'rk", "/srv/wo'rk")]);
        assert!(argv.ends_with(&[
            "--".into(),
            "/bin/bash".into(),
            "--noprofile".into(),
            "--norc".into()
        ]));
    }
}
