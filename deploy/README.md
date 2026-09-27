# Deploy Awaken

This document is the deployment and operator entry point. Local evaluation is
owned by the repository [quickstart](../README.md#try-awaken); k3d scenarios are
distributed verification fixtures rather than a second installation path.

## Process modes

```console
awaken all-in-one --no-browser       # Control + Coordinator + local Worker
awaken control --config /etc/awaken/control.toml
awaken coordinator --config /etc/awaken/coordinator.toml
awaken-worker --config /etc/awaken/worker.toml
awaken database migrate --config /etc/awaken/control.toml
awaken config --json                 # redacted effective configuration
```

`awaken` owns AllInOne, Control, and Coordinator composition. Execution-only
Worker deployment uses the separate `awaken-worker` binary; there is no
overlapping `awaken worker` compatibility command.

The root Compose quickstart intentionally selects `sandbox_tier = "local"` for
single-user evaluation, with the non-root Management container as the outer
boundary. Shared or multi-tenant deployments must select an isolated Namespace
or Kubernetes sandbox and provide its required runtime policy instead of
reusing that evaluation preset.

Production configuration comes only from an explicit `--config` path or the
standard `~/.awaken/config.toml`, followed by typed defaults. `AWAKEN_*`
environment variables are not deployment, model, business, or credential
configuration sources.

## Release evidence

The release-built Management image carries its original archive verification
manifest at `/usr/share/awaken/verification.json`, read-only to the application
user. The one release workflow validates that manifest before staging and
compares both its bytes and the packaged executable against new and reused
images before promotion. Both image paths pull and check the final immutable
digest and run E2E against that same subject. New images are uploaded to staging
before this check; a failed check leaves an unsigned staging image, never a
promoted release. A prior check of a mutable staging tag cannot authorize signing
another push result. Image signing binds those included bytes; consumers
must still authenticate the exact image digest and signing workflow identity.
The file is evidence of the release profile and its declared proof boundaries,
not a runtime Verified badge. Deployment TCB authentication, freshness/check-use
enforcement and frozen Session evidence remain required for Verified operation.

## Local and shared storage

Kubernetes deployments may use the standard [preallocated local volume
deployment](storage/local-pv/README.md) for ordinary PVCs. Its explicit StorageClass
keeps supply and recycling in the storage controller; application initialization,
data retention and recovery retain their existing owners.

All embedded databases and the generated `control-seal.key` live under the one
data directory. Local mode migrates embedded stores at startup. Shared server
deployments run `awaken database migrate` before application Pods; application
startup then verifies the existing schema without writing DDL. Shared
deployments select Postgres with `runtime_database_url`,
`resource_database_url`, and the per-component Control database fields shown by
`awaken config`; database URLs and key material are always redacted.

The typed configuration file can hold bootstrap settings:

```toml
data_dir = "/srv/awaken"
bind = "127.0.0.1:8080"
mode = "local"
run_local_pool = true
no_browser = false
```

The file also accepts `runtime_database_url`, `resource_database_url`, and
`catalog_db` / `credential_db` / `config_db` / `admin_db` / `data_subject_db` /
`environment_db` / `sessions_db` / `captured_content_db` for server deployments.
A deployment using one PostgreSQL authority for the complete AllInOne management
plane may instead set `management_database_url_file` to an operator-projected
secret file. It cannot be mixed with the per-store URL fields. Seal keys and
Cloud workload credentials likewise use their existing file-backed settings.

## Browser AI SDK origin admission

Cross-origin browser access to `/v1/ai-sdk` is disabled by default. An
AllInOne or Coordinator deployment may admit the exact Workspace UI origins
that are allowed to present short-lived application-access Bearer tokens:

```toml
ai_sdk_browser_origins = [
  "https://agents.example.com",
  "https://agents-admin.example.com:8443",
]
```

Each entry is a canonical origin, not a URL prefix: HTTPS is required except
for loopback HTTP during local development, and paths, wildcards, credentials,
query strings, fragments, duplicates, and more than 64 entries fail startup.
The policy is installed only on the AI SDK application router; it does not
grant access to AG-UI, Managed, authoring, or management routes. Actual
requests still pass the application-token guard and cookies are never allowed.
The preflight contract permits only `GET`/`HEAD`/`POST` with
`Authorization`, `Content-Type`, and `Accept`, exposes the AI SDK stream marker
and cache policy, and caches a successful preflight for at most ten minutes.
Do not add a wildcard CORS policy at an ingress proxy; if a proxy also emits
CORS headers, its allowlist must be identical and verified end to end.

## Split Worker

A split Worker has database-free execution configuration. Credential material
is projected into its trust domain by Kubernetes Secret, Vault CSI, or an
equivalent secret-volume provider:

```toml
role = "worker"
mode = "server"
worker_server = "https://awaken-coordinator:8443"
worker_server_ca_certificate_file = "/run/awaken/worker-server-ca.pem"
worker_request_credential_file = "/run/awaken/worker-request-credential.json"
worker_credential_material_root = "/run/awaken/credentials"
worker_credential_trust_domain = "awaken.worker"
```

The Worker rejects Control/Coordinator/Resource database URLs, Control seal-key
settings, and private Control-to-Coordinator tokens. Registration installs the
claim-fenced File, Memory, Skill, and Repository-verification clients; advertised
capabilities derive from those installed adapters and fail closed when required
adapters are absent.

### Local host ACP on a split Worker

A workstation Worker can opt into its installed Codex/Claude default account
without sending login material to Control. Add these fields to its Worker file:

```toml
data_dir = "/var/lib/awaken/local-worker"
sandbox_tier = "namespace"
sandbox_trusted_host_enabled = true
acp_clis = ["codex", "claude"]
worker_local_credential_bindings_file = "/run/awaken/local-bindings.json"
```

Use platform-native absolute paths on Windows/macOS. The binding file contains
only a JSON array of exact existing Control credential references:

```json
[
  {"credential":{"id":"<existing WorkerLocal source id>","revision":1},"cli_id":"codex","enabled":true}
]
```

The trusted external publisher derives this data through
`awaken_acp_application::project_local_credential_sources`; it must replace
the entire file atomically and protect its directory from workloads. The Worker
only consumes this projection. It neither creates Control intent nor opens a
Control database or CLI authentication file. This is an external projection
integration, **not an automatic Control-to-Worker delivery service**. The default
account is the only supported subject; another subject fails explicitly.

Startup uses the shared host discovery, pinned wrapper acquisition and protocol
probe, storing probe/wrapper directories under `data_dir`. It fails if none of
the explicitly selected host CLIs is routable. Without a binding file, the host
can publish protocol evidence for Provider-owned ACP but cannot advertise
WorkerLocal login authority. Container tiers reject host binding projections.
No ACP selection leaves the existing Native Worker path unchanged.

Every login refresh and exact execution-time check reloads the complete
projection, probes through the CLI, and reads it again. Missing files/empty
arrays revoke bindings; malformed, oversized, unreadable or unexpected entries
fail closed. A changed requested binding during the probe denies use; changed
batch metadata denies heartbeat refresh. The existing Worker cache still owns
observation expiry: failed refreshes do not extend old deadlines. A successful
check is not instant revocation protection during the final check-to-launch
interval, nor protection from a trusted publisher replaying older content.

The portable gate runs `CODEX_ACP_PROFILE=worker-host-fixture` through the
existing `e2e/codex_acp_live_e2e.mjs` resource mission for Codex and Claude. It
first lets normal startup create the source, then removes ACP from the embedded
Worker and launches the production `awaken-worker` artifact with a separate
installation directory, explicit private CA and signed request credential.
An unsigned registration must fail; the exact standalone registry row must
publish and revoke its projected reference without restarting. The same File,
Memory and Repository mission checks default/nested cwd, rendered resource
variables, independent bytes and Git patch application, concurrent Memory
conflicts, graceful/abrupt Worker replacement while Control stays alive, and
physical cleanup plus durable tombstones. The physical marker must reside in
the standalone Worker's root, excluding embedded execution as a passing oracle.

This is a same-host fixture qualification, not real vendor login, HTTPS Git
credential, cross-host cloud failure or automatic projection delivery proof.
The fixture retains the default 30-second observation TTL. The Worker validates
one timing contract: positive probe interval and, after each Coordinator lease
receipt, `observation TTL > probe interval + heartbeat interval + request budget`.
An incompatible initial receipt withdraws registration before claims; a later
incompatible lease drains and terminates that incarnation. Raise the authored
observation TTL to cover the negotiated budget; the Worker never extends old
evidence to hide configuration errors. Slow probes, scheduling stalls and
transport retries can still exhaust evidence and correctly fail closed.

## Split service boundaries

Split Control and Coordinator processes each bind a second private HTTP surface.
`bind` serves public/product routes; `internal_bind` serves authenticated service
routes. Expose each internal port through a separate ClusterIP Service without
Ingress and restrict callers with NetworkPolicy.

```toml
# Control configuration
role = "control"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
coordinator_internal_url = "http://awaken-coordinator-private:8081"
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"

# Coordinator configuration; keep this in a separate file.
role = "coordinator"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
control_internal_url = "http://awaken-control-private:8081"
control_service_token_file = "/var/run/secrets/awaken/control-service-token"
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"
```

Split roles fail startup when `internal_bind` is absent, malformed, or equal to
`bind`; AllInOne may select it for external Workers, while Worker rejects it.
Token values are loaded from files and
never appear in `awaken config`. `awaken database migrate` also prepares the
Coordinator-owned executable Agent command log when `runtime_database_url` is
configured.
