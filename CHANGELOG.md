# Changelog

All notable changes to Awaken are documented in this file.

## [1.0.0] - 2026-08-15

### Added

- Ship one `awaken` executable with an embedded web console and local
  `all-in-one` startup, plus role-specific Control and Coordinator commands.
- Add durable managed Agents, Environments, Sessions, Deployments, native agent
  execution, ACP execution, and transcript-prefix continuation.
- Add independently deployable Control, Coordinator, and Worker services with
  PostgreSQL-backed recovery, worker failover, and claim-fenced sandbox state.
- Add local, container, and Kubernetes sandbox execution with exact capability
  and resource publication.
- Add workspace-scoped IAM, credential vaulting, brokered model access, and
  least-authority service boundaries.
- Publish verified Linux, macOS, and Windows archives with SHA-256 checksums and
  GitHub build-provenance attestations.

### Changed

- Replace the earlier standalone/server split with the canonical `awaken`
  launcher and its shared role startup path.
- Make `awaken database migrate` the sole schema writer for shared deployments;
  application startup now verifies existing schemas.
- Freeze selected Agent, Environment, resource, credential, and policy
  references before dispatch so retries and recovery use the same inputs.

### Security

- Separate application, service, worker, and management credentials and reject
  authority-owned database or secret settings at the Worker boundary.
- Add authenticated private Control-to-Coordinator surfaces, exact workspace
  authorization, credential-use binding, and fail-closed capability admission.

[1.0.0]: https://github.com/awakenworks/awaken/releases/tag/v1.0.0
