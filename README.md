# Awaken Agents

Build AI applications with persistent sessions and tool permissions.

Awaken Agents is an open-source, self-hostable execution platform for AI agent
applications. Start its local Console, publish an Agent, and connect your
frontend or backend through supported APIs. Awaken manages execution, committed
Session history, resources and tool permissions; your application keeps its
experience, business rules and result acceptance.

## What you can build

- **Assistants and copilots:** connect a product UI or backend to a published
  Agent and reopen the same Session across connections.
- **Repository and research applications:** bind the files, tools, skills and
  model access a task needs, then inspect its result and recorded progress.
- **Multiple Agent applications:** reuse publication, permissions and execution
  across applications while each keeps its own interface and business rules.

Use Awaken when your application needs durable Agent work and an explicit
execution boundary. A single prompt or a trusted one-off script may need less
infrastructure. Recovery follows committed facts and configured policy; an
uncertain external action must not be blindly replayed.

## Choose your starting point

| You want to | Start here |
|---|---|
| Run an Agent service and connect your application | [Try Awaken](#try-awaken), then [build with Awaken](#build-with-awaken) |
| Embed the execution core in a Rust application | [Runtime SDK](crates/sdk/runtime/awaken-runtime/README.md) and its [independent consumer](examples/sdk-consumer/README.md) |
| Self-host and operate the service | [Operate Awaken](#operate-awaken) |
| Contribute to Awaken | [CONTRIBUTING.md](CONTRIBUTING.md) and the [engineering docs](docs/README.md) |

An Agent defines published behavior; an Environment describes execution
requirements. A Session holds ongoing work and committed history, and a Run
executes work within that Session.

## Availability

This guide describes the development source in its own checkout. Use the source
path below for this version. Public [Releases](https://github.com/awakenworks/awaken/releases)
can belong to an older release line; use documentation matching the package you
install.

## Build from source

Use a checkout of the revision you intend to evaluate. You need Rust 1.96.0
(from [rust-toolchain.toml](rust-toolchain.toml)), Node.js 22 (used by
[CI](.github/workflows/quality.yml)), and pnpm 11.6.0 (from [package.json](package.json)). Cargo's
existing Console build step installs the locked Node dependencies and embeds the
built Console in the executable:

```console
cargo build --locked --release -p awaken-cli --bin awaken -p awaken-sandbox --bin awaken-sandbox --features awaken-sandbox/hand
./target/release/awaken --version
./target/release/awaken-sandbox hand --check
./target/release/awaken doctor
```

Keep `awaken` and its matching `awaken-sandbox` execution companion together.
These are the same binary targets and Hand feature used by the release builder.
The built program needs no Node.js or external web directory at runtime. On
Linux, Namespace execution needs usable bubblewrap (`bwrap`); read the doctor's
result before choosing an execution tier. The deployment guide owns the
[platform and isolation choices](deploy/README.md#local-execution-tiers).

## Try Awaken

### 1. Start the local service

From the same checkout:

```console
./target/release/awaken --no-browser
```

Open the local URL printed at startup, normally <http://127.0.0.1:8080>. Follow
the one-time setup link or token if startup prints one. AllInOne starts the API,
embedded Console and local Worker together, and keeps local state under
`~/.awaken`. Use `--port` for an occupied port or `--data-dir` for a dedicated
evaluation directory. Once installed on your PATH, `awaken` starts the same mode.

### 2. Create and publish an Agent

In the Console:

1. Connect a model provider, or sign in to a detected, supported ACP runtime.
2. Open **Quickstart**. Choose **Task assistant** or describe your Agent.
3. Review the draft, model and Environment before publishing it.
4. Use a small first task, for example: “Reply with a two-item checklist for
   reviewing an API change.” Review, publish and start the Session.

A description or starter produces an editable draft. Review and publish the
Agent to make that revision available for execution in the selected Environment.

### 3. Inspect one result

Open the Session and confirm that the Agent answered the task and returned to
idle. Inspect the committed messages and any tool or permission activity.
An application still decides whether that answer meets its own quality bar.

| What you see | Next action |
|---|---|
| Provider or model is not ready | Correct the connection or login in the Console; check its recorded error |
| Work stays queued | Check Environment readiness and whether an eligible Worker is available |
| Work waits for approval | Review and answer the existing permission request |
| A permanent admission refusal or terminal error | Inspect the recorded cause and correct the inputs; repeated submission is not recovery |
| An external result is indeterminate | Reconcile that action's actual outcome before retrying it |

Stop with `Ctrl-C`, restart with the same command and data directory, and reopen
the same Session. The earlier committed history should still be there. This
checks persistence. Continuing an in-flight task also depends on its configured
recovery policy and the outcome of any external action.

## Build with Awaken

After the first task, open the Console's **Integrate** step or **API & protocols**.
Use its contextual example with the published Agent and Environment IDs:

| Your application | Existing interface |
|---|---|
| A trusted backend starting and following Agent work | Managed Agents API and the reviewed official SDK contract |
| A web frontend using AI SDK | Vercel AI SDK message stream |
| An Agent UI using AG-UI or CopilotKit | AG-UI |
| Another Agent exchanging tasks | A2A |
| An MCP client calling explicitly exported tools | MCP |

The Console provides contextual setup and copyable client examples, including
the [Managed SDK example](web/src/lib/sdk-examples.ts). Before migrating an
existing client, check the [SDK compatibility record](packages/managed-sdk-oracle/README.md)
for reviewed versions, supported operations and qualification boundaries.

Follow the Console's identity-specific instructions for credentials. Keep a
Workspace service API key in a trusted backend; browser and mobile applications
use scoped, short-lived application access. See [application authentication](docs/application-authentication.md).

Native execution, supported ACP runtimes and outbound A2A are execution choices,
separate from the client protocol. Files, MemoryStores, Skills and tool
permissions remain resource and execution bindings rather than application
business state.

For a Rust process embedding execution directly, start with the [Runtime SDK](crates/sdk/runtime/awaken-runtime/README.md)
and [independent consumer](examples/sdk-consumer/README.md). Internal
[runtime examples](crates/sdk/devtools/awaken-runtime-examples/examples/README.md)
teach composition mechanics; they are an advanced path after the public SDK.

## Operate Awaken

The [deployment guide](deploy/README.md) owns configuration, execution tiers,
AllInOne and split processes, migration, credentials, backups and operating
boundaries. Local evaluation and a shared production deployment have different
requirements. k3d material is a distributed verification topology.

For deployment or integration support, see [AwakenWorks](https://awakenworks.com/enterprise?product=agents#apply).

## Documentation and contribution

The [documentation index](docs/README.md) routes each reader to its canonical
owner. [CONTRIBUTING.md](CONTRIBUTING.md) explains implementation checks. Product
behavior belongs to code and executable contracts; the documentation model and
synchronization policy are owned by [ADR-0001](docs/adr/0001-documentation-model-and-vocabulary-alignment.md#amendment-2026-11-09-developer-entry-and-downstream-publication).

## License

Apache License, Version 2.0. See [LICENSE](LICENSE). Runtime protocol,
specification and conformance material use Apache-2.0; consult individual
package or file metadata for the code you adopt.
