//! `awaken-provisioning-contract` — the neutral, data-only vocabulary and ports
//! for provisioning a **sandbox environment** and launching processes in it.
//!
//! # Why a process-level contract
//!
//! The sandbox must host **opaque agent processes** (Claude Code, Codex, an
//! arbitrary CLI) — not only the runtime's own in-process tools. Such a process
//! does its own `open()`/`exec()`/network syscalls and never routes through our
//! tool layer, so isolation **cannot** be done by rewriting tool-call arguments.
//! It must be enforced at the **OS/process boundary** and be *transparent* to
//! whatever runs inside. Hence the primary primitive here is [`Sandbox::spawn`]
//! (launch any [`Command`] under OS-enforced isolation), not a tool wrapper. The
//! runtime's in-process `RawTool` model is a *special case* layered on top, owned
//! by other crates; a lexical path-jail is a trusted-caller convenience only and
//! is explicitly **not** an isolation boundary for a launched process.
//!
//! # Boundary discipline
//!
//! - **Data-only + ports.** This crate names no OS mechanism, no host path, no
//!   wire/DTO, and no runtime type. Concrete realizers (lexical / namespace /
//!   container) implement [`SandboxProvider`] in their own crates.
//! - **G3 — no host path crosses the boundary.** Every path in this vocabulary is
//!   *sandbox-absolute* (e.g. `/workspace`, `/mnt/session/outputs`) or a logical
//!   reference; raw host paths live only inside a provider.
//! - **Secrets by reference.** A secret env var carries a broker *reference*
//!   ([`EnvValue::Secret`]), never the secret bytes.
//!
//! The two planes are named distinctly: [`EnvironmentKind`] is the *declared*
//! control-plane config (persisted, admitted); a live realized environment is the
//! provider's own type behind [`Sandbox`].

mod admission;
mod approval;
mod lease;
mod poison;
mod prepare;
mod sandbox;
mod shape;
mod spec;
mod vocab;

pub use admission::{AdmissionError, EnvironmentDecl, check_environment_soundness};
pub use approval::{ApprovalDecision, ApprovalPolicy, SandboxAction, decide as approval_decide};
pub use lease::{
    AdoptionPlan, LeaseGrant, LeaseLiveness, LivenessSignals, ReapCause, ReconcileOutcome,
    apply_adoption_plan, capped_expiry, decide_reap, egress_permitted, reconcile_adoption,
    reconcile_and_apply,
};
pub use poison::{AttemptSignal, PoisonVerdict, classify as classify_poison};
pub use prepare::{EnvironmentPlan, PrepareError, prepare_environment};
pub use sandbox::{
    BlobSource, ExitStatus, IsolationClass, MemoryMount, MemoryMounter, ProcessHandle, Sandbox,
    SandboxCapabilities, SandboxError, SandboxHandle, SandboxProvider, SandboxStatus,
    SelectionError, Signal, select_provider,
};
pub use shape::{ExecutionShape, plan_shape};
pub use spec::{Command, EnvironmentKind, RootfsSource, SandboxSpec, Stdio};
pub use vocab::{
    Artifact, EnvValue, EnvVar, EnvVisibility, MountAccess, MountLifetime, MountRequirement,
    MountSource, NetworkPolicy, RESERVED_ENV_KEYS, Realization, RealizedMount, ResourceLimits,
};
