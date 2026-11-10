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

Checked on 2026-11-10, based on Native main `6b7df82139` plus this worktree's
changes. The test cases contain their cause/effect rules. Checks run on Linux;
other operating systems are unverified by this run.

| Example | Executed checks |
|---|---|
| Node + React | HTTP client/backend tests, typecheck and build; production Awaken first installation, two Node turns, history, mobile browser and reload; real DeepSeek execution and an inspectable API review |
| Independent Rust consumer | All 8 offline consumer tests |
| `hello_agent`, `direct_runtime` | Both programs executed; their smoke tests passed |
| Coding TUI/core | 13 focused tests; real model approved an edit and denied another, with file contents checked |
| Console examples | Protocol tests and recording source contracts; generated README consistency |

The browser Stop refusal/acknowledgment branches use a separate controlled
protocol fixture. Real-provider success does not turn those fixture results
into provider execution evidence. Product recordings have not been rerun for
this change.

Separately owned reference applications were checked from committed source
snapshots; existing work in their original checkouts was preserved:

| Application | Source revision | Checks run | Remaining Native integration check |
|---|---|---|---|
| Cowork | `396f8051112f` | 214 unit tests, build, 52 oracle tests; 9 service journeys against this Native with a deterministic model | Real-provider application journey; a cancelled cleanup case intentionally retained fenced evidence |
| Pilot | `973dc96aa204` | Typecheck, 126 unit tests, build | Full live runner needs IAM and provider setup; its pinned model needs reconciliation with current discovery |
| DeepSeek Harness | `3faa59acc126` | Typecheck, 42 unit tests, build, 10 fixture browser journeys | Real-provider journey against this Native |
| Design | `42b47097162a` | Typecheck, 464 unit tests, build; 40 PostgreSQL/MinIO/browser integration tests | Optional Native publication gate was skipped (1 test) |

These checks establish the stated layers only. Follow each application's own
setup and acceptance instructions before treating it as a deployment template.
