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

The separately owned repairs below are integrated into each application's local
main; original checkout work was preserved. No branch or public case/source
archive was pushed or published. Provider journeys use real DeepSeek
`deepseek-flash` on Linux.

Cowork, Pilot and Harness were checked against Native
`a95f392e0243899fab673defb38c5366dadd0bc0`. Design was rebased over its newer
main `297522b` and rechecked against Native `9d2b32d988`, whose intervening change
is documentation only. The Design check retains main's transient-history
recovery and the shared smoke/corpus terminal guard.

| Application | Repair revision | Source and controlled checks | Real-provider check |
|---|---|---|---|
| Cowork | `b378ede` | 214 unit, 54 oracle/helper tests, 62 browser journeys and build | Short and explicit headcount input both deliver verified Files on Namespace; independent arithmetic/status and approval limits retained |
| Pilot | `14d831e` | 137 unit/runner tests, typecheck and build | Complete schedule/resources, desktop/mobile browser, both research children, Plan, satisfied Outcome, artifact and real IAM share/revoke journey on Namespace |
| DeepSeek Harness | `c670e1f` | 46 unit tests, typecheck/build and 12 browser/video journeys | Complete preset/Skill/reconnect/plan/question/creator journey; approved child Bash, cross-Thread reply and later parent report on Namespace |
| Design | `7725193` | 487 source tests and build; 53 PostgreSQL/MinIO/controlled-browser tests (live case explicitly skipped in that lane) | Rebased real Docker gate passed 29 checks, including initial/child generation, verified Preview, acceptance and ZIP export; missing-browser negative fails promptly |

The application READMEs own preparation, the first useful result and verification
commands; detailed records retain exact runtime and failure boundaries. Repair
coverage includes refused child confirmation/retry, wrong output keys before
publication, Native initialization refusal, missing model selection, invalid
Project event order, SDK capability-header conflicts, unsuccessful Web tools and
a transient history read followed by exact terminal evidence.
Tests keep their cause/effect design beside the assertions.

When changing a Managed contract, Skill, model selection or Sandbox toolchain,
rerun the affected application's source checks and its real boundary journey;
record its application/runtime revisions and environment before updating this
table. A fixture pass or historical public archive cannot substitute for that
journey. Native's existing documentation gate checks the local links and generated
example documentation.

These are bounded local checks. The full office matrix, all deployment platforms,
Design's 100-case semantic quality and customer acceptance remain separate scopes.
The Native cancellation negative retains its existing fenced evidence; it is not
claimed as confirmed physical cleanup. Follow each application's own setup and
acceptance guide before treating it as a deployment template.
