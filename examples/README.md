# Examples and next steps

Choose one path. Application examples use Awaken's existing Session and protocol
APIs; SDK examples embed Runtime directly in a Rust process.

| Goal | Maintained source | First result |
|---|---|---|
| Connect a trusted Node backend | [Application example](application/README.md#backend-read-a-reply-then-reuse-its-session) | A reply and a second message in the same Session |
| Add a React chat | [Browser example](application/README.md#browser-use-the-same-history-through-ai-sdk) | Backend-issued token, streamed answer and history after refresh |
| Embed Runtime in Rust without model credentials | [Independent consumer](sdk-consumer/README.md) | A committed deterministic reply; its tests cover tools, approval and restart |
| Learn low-level Runtime assembly | [hello_agent and direct_runtime](../crates/sdk/devtools/awaken-runtime-examples/examples/README.md) | Offline natural terminal state and committed transcript |
| Build a terminal coding agent | [Coding TUI](../crates/sdk/devtools/awaken-runtime-examples/examples/coding_agent/README.md) | Read a file, approve or deny an edit, inspect the result |
| Inspect SDK/protocol behavior | [Managed lifecycle test](../e2e/managed_e2e.mjs), [AI SDK test](../e2e/ai_sdk_e2e.mjs) and [application-authentication test](../e2e/application_auth_e2e.mjs) | Existing wire/authorization regressions, not application dependencies |

## Full reference applications

These are separately owned applications with broader product behavior. They are
not required by the first-use examples above, and their historical evidence does
not qualify a different Awaken revision.

- [Pilot](https://awakenworks.com/cases/pilot): a Mission-oriented application.
- [DeepSeek Harness](https://awakenworks.com/cases/deepseek-harness): a workbench
  with trajectories, approvals, presets and deliverables. Its fixed walkthrough
  uses a deterministic Managed API fixture.
- [Design](https://awakenworks.com/cases/design): an application with its own
  Project, Revision, review and artifact publication model.
- Cowork: an internal React reference for uploads, tasks, approvals and file
  delivery. Its source is maintained separately; no public source URL is claimed.

Source availability and validation level are stated by each reference's own
release/case page. The [existing recording system](../web/record/README.md) owns
product-story recordings; its catalogue is not evidence that every current
chapter has been executed or published.

## Validation

Each example README owns its preparation and executable checks. Keep causal
test design beside the actual assertions. HTTP fixtures establish their
controlled branch; real-provider, deployment and application-quality checks
retain separate evidence and require their actual configuration.

Record source/runtime revisions, environment, failures and execution packets
outside tracked source using the existing evidence owner. A skipped fixture or
historical result does not establish a new successful journey. Separately owned
applications retain their own setup, release and acceptance authority.

The existing documentation entrypoints check local links and the README
projection from its runnable source; Console consumes that same source.
