# Awaken Agents

This repository hosts Awaken Agents and its `awaken-runtime` execution core.
The runtime protocol/specification and conformance surface are licensed under
Apache-2.0; code packages may use their own file or package license metadata.

## Install

Release archives contain the `awaken` executable, its matching
`awaken-sandbox` execution companion, this README, the Docker Compose
quickstart, and the Apache 2.0 license. The `awaken` executable already contains
the web console; Node.js is not required at runtime.

Linux x86-64 and macOS (Apple Silicon or Intel) can use the same installer. It
selects the release target, verifies the archive SHA-256, validates the binary's
exact version, and installs to `${XDG_BIN_HOME:-$HOME/.local/bin}` without sudo:

```console
curl -fsSL https://github.com/awakenworks/awaken/releases/download/v1.0.0/install.sh | sh -s -- v1.0.0
awaken --version
```

Set `AWAKEN_INSTALL_DIR` to choose another destination. For provenance-sensitive
environments, download `install.sh` and `install.sh.sha256`, verify the script,
then run it; every release asset also has a GitHub build-provenance attestation.

Windows x86-64 can use the checksum-verifying PowerShell installer:

```powershell
$installer = Join-Path $env:TEMP 'install-awaken.ps1'
Invoke-WebRequest https://github.com/awakenworks/awaken/releases/download/v1.0.0/install.ps1 -OutFile $installer
powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer v1.0.0
awaken --version
```

For provenance-sensitive Windows installations, verify `install.ps1` against
`install.ps1.sha256` before running it. The full platform mapping is:

| System | Release target |
| --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x86-64 | `x86_64-pc-windows-msvc` |

Each archive has a sibling `.sha256` file. Verify it before extracting the
archive. GitHub also publishes a build-provenance attestation for every release
asset; with the GitHub CLI installed, verify it with
`gh attestation verify <archive> --repo awakenworks/awaken`.

To build from source, install Rust 1.96.0, Node.js, and pnpm 11.6.0, then run:

```console
pnpm install --frozen-lockfile
cargo build --locked --release -p awaken-cli --bin awaken
./target/release/awaken --version
```

## Try Awaken

Start the installed program:

```console
awaken
```

Running `awaken` without a command is the same as `awaken all-in-one`: it starts the
API, embedded web console, and local worker, persists all local state under
`~/.awaken`, and opens `http://127.0.0.1:8080`. Use `--port`, `--data-dir`, or
`--no-browser` for common local overrides. The resulting binary contains the
complete console and needs no external web directory or Node.js at runtime.

The default execution adapter follows the host's implemented capability:
Linux uses bubblewrap Namespace isolation, macOS uses Seatbelt Namespace
isolation, and Windows uses the host-backed Local adapter because Awaken has no
Windows Namespace adapter. Linux therefore requires a usable `bwrap`; run
`awaken doctor` for an actionable readiness result. Windows Local execution and
an explicitly configured Local tier have no OS isolation and are appropriate
only for trusted, single-user workstations.

To deliberately use programs already installed on the host on any platform,
set the one deployment authority in `~/.awaken/config.toml` (on Windows,
`%USERPROFILE%\.awaken\config.toml`):

```toml
sandbox_tier = "local"
```

The Local adapter launches ACP CLIs and shell commands as host child processes
with the Session workspace as their working directory. Files, repositories,
MemoryStores, Skills, and outputs still enter through Agent resource bindings;
Local mode does not expose arbitrary host paths or create a second resource
authority. Use an MCP server when an existing local service should be called
without filesystem access.

The Console guides one canonical path:

1. Connect a model provider or sign in to a detected ACP runtime.
2. Choose a starter and define one real task.
3. Review and publish the immutable Agent revision.
4. Follow the durable Session and its committed events.

Run the same product from the signed container image when a local binary is not
convenient:

```console
docker compose -f deploy/compose.yaml up -d
docker compose -f deploy/compose.yaml logs awaken
```

Open <http://127.0.0.1:8080> and use the one-time setup token printed in the
logs. The named volume preserves local state across container replacement.
This Compose preset is for single-user evaluation: the Management container is
the outer isolation boundary and its embedded Worker uses the local sandbox
tier. Use the operator configuration below for shared or multi-tenant systems.

Before startup, diagnose the selected configuration, data directory, and
listener without creating product state. ACP discovery remains available under
the same diagnostic command:

```console
awaken doctor
awaken doctor --json
awaken doctor acp
```

## Build with Awaken

After the first Session is running, open **API & protocols** in the Console for
copyable Managed Agents, Vercel AI SDK, AG-UI, A2A, and MCP examples. Every
protocol uses the same published Agent and durable Session authority.

Runtime-internal examples under `crates/devtools/` teach embedding and extension
mechanics; they are not a second product quickstart.

## Operate Awaken

All-in-one, split Control/Coordinator/Worker topology, typed configuration,
database migration, secret projection, and private service boundaries are owned
by [deploy/README.md](deploy/README.md). The k3d material is a distributed
verification topology, not the local onboarding path.

For implementation boundaries, runtime coverage, architecture invariants, and
documentation checks, continue to [docs/README.md](docs/README.md).

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
