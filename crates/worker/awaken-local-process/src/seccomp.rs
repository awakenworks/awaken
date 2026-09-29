//! The syscall profile bubblewrap installs on every sandboxed workload.
//!
//! bubblewrap already runs the workload unprivileged in fresh user, PID, IPC,
//! UTS (and, when egress is denied, network) namespaces with every capability
//! dropped and `no_new_privs` set. What those namespaces do not close is the
//! kernel surface an unprivileged process can still reach, so this profile
//! denies it with `EPERM`:
//!
//! - `io_uring_*`: an alternative submission path (`IORING_OP_SOCKET`, ...)
//!   that bypasses per-syscall filtering, and a recurring privilege-escalation
//!   surface;
//! - creating a user namespace (`unshare`/`clone` with `CLONE_NEWUSER`): a
//!   nested user namespace hands the workload a full capability set over fresh
//!   mount/network namespaces, which is where most unprivileged kernel exploits
//!   start;
//! - the kernel keyring (`keyctl`, `add_key`, `request_key`), which is not
//!   namespaced and so leaks between sandboxes of one user;
//! - `bpf`, `perf_event_open` and `userfaultfd`;
//! - `socket(2)` outside `AF_INET`, `AF_INET6` and `AF_NETLINK` — plus
//!   `AF_UNIX` unless the profile forbids it. `AF_VSOCK` in particular is not
//!   namespaced: a networkless sandbox could otherwise still reach host or
//!   hypervisor vsock services.
//!
//! `clone3` passes its flags in memory a BPF filter cannot read, so it returns
//! `ENOSYS` instead; libc then falls back to `clone`, whose flags are checked.
//! A syscall from a foreign ABI (i386 `int 0x80` on x86_64) kills the process,
//! and on x86_64 every denied syscall is also denied under its x32 number.
//!
//! The profile is compiled in process and handed to bubblewrap as sealed
//! memory file descriptors (`--add-seccomp-fd`); nothing is written to a path a
//! workload could race.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

pub use crate::{
    BUBBLEWRAP_SECCOMP_FDS as BUBBLEWRAP_FILTER_FDS, SECCOMP_PROGRAM_COUNT as PROGRAM_COUNT,
    bubblewrap_seccomp_args as bubblewrap_filter_args,
};

/// Which sockets a workload may create in addition to IP and netlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallProfile {
    /// The Namespace tier: `AF_UNIX` stays available for the runtime's own
    /// in-sandbox rendezvous and control sockets. Host sockets are withheld by
    /// not projecting them, not by this filter.
    Namespace,
    /// A launch that may reach every host path read-only (the Workdir tier's
    /// networkless shell): creating an `AF_UNIX` socket is denied, so the host's
    /// daemon sockets cannot be connected to. `socketpair(2)` is unaffected.
    NoUnixSockets,
}

/// The profile could not be compiled for, or handed to, this host.
#[derive(Debug)]
pub struct SyscallProfileError(String);

impl std::fmt::Display for SyscallProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox syscall profile: {}", self.0)
    }
}

impl std::error::Error for SyscallProfileError {}

const X32_SYSCALL_BIT: i64 = 0x4000_0000;
/// Families any profile admits: `AF_INET`, `AF_INET6`, `AF_NETLINK`
/// (`getaddrinfo`/`getifaddrs` need route netlink).
const IP_AND_NETLINK_FAMILIES: [libc::c_int; 3] = [libc::AF_INET, libc::AF_INET6, libc::AF_NETLINK];

impl SyscallProfile {
    fn admitted_socket_families(self) -> Vec<libc::c_int> {
        let mut families = IP_AND_NETLINK_FAMILIES.to_vec();
        if self == Self::Namespace {
            families.push(libc::AF_UNIX);
        }
        families
    }

    /// The `EPERM` rule table, keyed by native syscall number.
    fn denied_rules(self) -> Result<BTreeMap<i64, Vec<SeccompRule>>, SyscallProfileError> {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for syscall in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
        ] {
            rules.insert(syscall, Vec::new());
        }
        let new_user = u64::try_from(libc::CLONE_NEWUSER).map_err(invalid)?;
        for syscall in [libc::SYS_unshare, libc::SYS_clone] {
            rules.insert(
                syscall,
                vec![rule(vec![condition(
                    0,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::MaskedEq(new_user),
                    new_user,
                )?])?],
            );
        }
        let family_conditions = self
            .admitted_socket_families()
            .into_iter()
            .map(|family| {
                let family = u64::try_from(family).map_err(invalid)?;
                condition(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, family)
            })
            .collect::<Result<Vec<_>, _>>()?;
        rules.insert(libc::SYS_socket, vec![rule(family_conditions)?]);
        Ok(with_x32_aliases(rules))
    }

    /// The compiled BPF programs, in installation order.
    pub fn compile(self) -> Result<Vec<BpfProgram>, SyscallProfileError> {
        let arch = native_arch()?;
        let denied = SeccompFilter::new(
            self.denied_rules()?,
            SeccompAction::Allow,
            SeccompAction::Errno(errno(libc::EPERM)?),
            arch,
        )
        .map_err(invalid)?;
        let clone3 = SeccompFilter::new(
            with_x32_aliases(BTreeMap::from([(libc::SYS_clone3, Vec::new())])),
            SeccompAction::Allow,
            SeccompAction::Errno(errno(libc::ENOSYS)?),
            arch,
        )
        .map_err(invalid)?;
        let programs: [SeccompFilter; PROGRAM_COUNT] = [denied, clone3];
        programs
            .into_iter()
            .map(|filter| BpfProgram::try_from(filter).map_err(invalid))
            .collect()
    }

    /// One sealed, rewound memory file per compiled program, ready to be mapped
    /// into bubblewrap and named by `--add-seccomp-fd`.
    pub fn bubblewrap_filter_fds(self) -> Result<Vec<OwnedFd>, SyscallProfileError> {
        self.compile()?.iter().map(sealed_program_fd).collect()
    }
}

fn native_arch() -> Result<TargetArch, SyscallProfileError> {
    TargetArch::try_from(std::env::consts::ARCH).map_err(|_| {
        SyscallProfileError(format!(
            "no syscall profile for architecture `{}`",
            std::env::consts::ARCH
        ))
    })
}

fn invalid(error: impl std::fmt::Display) -> SyscallProfileError {
    SyscallProfileError(error.to_string())
}

fn errno(value: libc::c_int) -> Result<u32, SyscallProfileError> {
    u32::try_from(value).map_err(invalid)
}

fn condition(
    index: u8,
    length: SeccompCmpArgLen,
    operator: SeccompCmpOp,
    value: u64,
) -> Result<SeccompCondition, SyscallProfileError> {
    SeccompCondition::new(index, length, operator, value).map_err(invalid)
}

fn rule(conditions: Vec<SeccompCondition>) -> Result<SeccompRule, SyscallProfileError> {
    SeccompRule::new(conditions).map_err(invalid)
}

/// x86_64 also admits the x32 ABI under the same audit architecture, with the
/// syscall number offset by bit 30. Deny every rule under both numbers.
fn with_x32_aliases(rules: BTreeMap<i64, Vec<SeccompRule>>) -> BTreeMap<i64, Vec<SeccompRule>> {
    if !cfg!(target_arch = "x86_64") {
        return rules;
    }
    let aliases: Vec<_> = rules
        .iter()
        .map(|(syscall, chain)| (syscall | X32_SYSCALL_BIT, chain.clone()))
        .collect();
    let mut rules = rules;
    rules.extend(aliases);
    rules
}

/// Serialize `struct sock_filter[]` exactly as the kernel (and bubblewrap)
/// read it: `u16 code, u8 jt, u8 jf, u32 k` in native byte order.
fn program_bytes(program: &BpfProgram) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(program.len() * 8);
    for instruction in program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.push(instruction.jt);
        bytes.push(instruction.jf);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    bytes
}

fn sealed_program_fd(program: &BpfProgram) -> Result<OwnedFd, SyscallProfileError> {
    use rustix::fs::{MemfdFlags, SealFlags, SeekFrom};
    let fd = rustix::fs::memfd_create(
        "awaken-seccomp",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(invalid)?;
    let bytes = program_bytes(program);
    let mut written = 0;
    while written < bytes.len() {
        written += rustix::io::write(&fd, &bytes[written..]).map_err(invalid)?;
    }
    rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
    )
    .map_err(invalid)?;
    rustix::fs::seek(&fd, SeekFrom::Start(0)).map_err(invalid)?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Profile rule table. Causes: P profile (Namespace / NoUnixSockets).
    /// Effects: E1 both profiles deny io_uring, keyring, bpf, perf, userfaultfd
    /// unconditionally; E2 unshare/clone are denied only under CLONE_NEWUSER;
    /// E3 socket admits IP + netlink, and AF_UNIX only under Namespace; E4 on
    /// x86_64 every denied number has an x32 alias; E5 both programs compile
    /// and serialize to whole `sock_filter` records.
    #[test]
    fn profile_rule_table_matches_the_documented_policy() {
        for profile in [SyscallProfile::Namespace, SyscallProfile::NoUnixSockets] {
            let rules = profile.denied_rules().unwrap();
            for unconditional in [
                libc::SYS_io_uring_setup,
                libc::SYS_io_uring_enter,
                libc::SYS_io_uring_register,
                libc::SYS_keyctl,
                libc::SYS_add_key,
                libc::SYS_request_key,
                libc::SYS_bpf,
                libc::SYS_perf_event_open,
                libc::SYS_userfaultfd,
            ] {
                assert!(rules[&unconditional].is_empty(), "E1 {unconditional}");
            }
            for conditional in [libc::SYS_unshare, libc::SYS_clone, libc::SYS_socket] {
                assert_eq!(rules[&conditional].len(), 1, "E2/E3 {conditional}");
            }
            let families = profile.admitted_socket_families();
            assert!(families.contains(&libc::AF_INET) && families.contains(&libc::AF_INET6));
            assert!(families.contains(&libc::AF_NETLINK));
            assert!(!families.contains(&libc::AF_VSOCK));
            assert_eq!(
                families.contains(&libc::AF_UNIX),
                profile == SyscallProfile::Namespace,
                "E3 {profile:?}"
            );
            if cfg!(target_arch = "x86_64") {
                for syscall in rules.keys().filter(|nr| **nr < X32_SYSCALL_BIT) {
                    assert!(rules.contains_key(&(syscall | X32_SYSCALL_BIT)), "E4");
                }
            }
            let programs = profile.compile().unwrap();
            assert_eq!(programs.len(), PROGRAM_COUNT, "E5");
            assert_eq!(bubblewrap_filter_args().len(), 2 * PROGRAM_COUNT, "E5");
            for program in &programs {
                assert!(!program.is_empty());
                assert_eq!(program_bytes(program).len(), program.len() * 8, "E5");
            }
        }
    }

    #[test]
    fn filter_fds_are_sealed_and_rewound() {
        use std::io::Read as _;
        let fds = SyscallProfile::Namespace.bubblewrap_filter_fds().unwrap();
        let programs = SyscallProfile::Namespace.compile().unwrap();
        assert_eq!(fds.len(), programs.len());
        for (fd, program) in fds.into_iter().zip(&programs) {
            let seals = rustix::fs::fcntl_get_seals(&fd).unwrap();
            assert!(seals.contains(rustix::fs::SealFlags::WRITE));
            assert!(seals.contains(rustix::fs::SealFlags::SEAL));
            assert!(
                rustix::io::write(&fd, b"x").is_err(),
                "sealed against writes"
            );
            let mut bytes = Vec::new();
            std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, program_bytes(program), "rewound to the first record");
        }
    }
}
