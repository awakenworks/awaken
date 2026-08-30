# Marketing and product-proof coverage

This is the release-truth map between the earlier `/Users/chaizhenhua/Codes/awaken`
video library and the current all-in-one release. “Covered” means either a public story
closes a human intent → operation → effect → handoff loop, or a proof-only test verifies
a technical contract without pretending it is a marketing story. A script that is never
executed, a settings tour, or a gated page is not coverage.

## Closure gate

Every publishable chapter must answer all four questions:

1. **Intent**: what outcome is the viewer trying to achieve?
2. **Operation**: which current product control or public protocol performs it?
3. **Effect**: what durable or runtime state changed?
4. **Readback**: which independent UI/API observation proves the effect and its boundary?

A failed public-story dependency publishes only a diagnostic artifact. A failed proof
returns a non-zero test result. Neither inherits an old MP4 or downgrades a runtime claim
to page copy.

## Earlier content carried forward

| Earlier chapter/theme | Current owner | Closure evidence | Release state |
| --- | --- | --- | --- |
| V0 Install / one-binary startup | 07 | Clean data directory, copied binary starts, `/readyz` and embedded Console respond from one listener | Proof-only installation gate |
| V1 Build / V4 Tools / V5 HITL | 02, 03, 06 | Publish diff and live result; exact tool identity denied; State Machine blocks before tool execution | 02 is public; 03 and 06 are proof-only |
| V2 What is Awaken / V14 Durable | 00, 13, 14 | Agent → Environment → Session → Event contract; API-created Session appears in Console; archive rejects later writes | 00 is the public opener; 13 and 14 are proof-only |
| V3 Tune prompt / V6 authoring | 02, 05 | Draft preview, reviewed publication, AI-proposed config diff under human activation | 02 is a public story; 05 is proof-only |
| V7 Integrate / W2 Embed Agent | 03, 13, 15, 17, 18 | Official Anthropic SDK request and Console identity; public A2A Card; AI SDK + AG-UI shared history; authenticated MCP tool discovery | 03 is public; the remaining protocol mechanics are proof-only |
| V8 Provider | 01 | Provider Connection verification and exact `MODEL READY` model output | Proof-only; requires explicit authorization for an external provider credential |
| V9 Trace / V13 History restore | 04, 11, 14 | Real source read trace and approved artifact; Session Inputs provenance; interrupt/archive receipts | 04 is public; the remaining lifecycle mechanics are proof-only |
| V10 Evals | Product-gated | Dataset/evaluation result visible in Console and tied to a committed Agent version | Not release-ready; excluded instead of simulated |
| V11 Skills | 10, 11 | Bound Skill changes live output; realized Skill/file/Memory inputs remain inspectable | Proof-only until the capability produces a customer deliverable |
| V12 MCP | 03, 18 | Runtime MCP permission effect plus protected Awaken MCP server discovery | Proof-only until the capability closes a customer workflow |
| V15 Capstone / V16 Admin assistant | Series journey, 05 | Combined feature map; Assistant produces a reviewable configuration proposal | Overview is public; Assistant authoring is proof-only |
| ACP/Codex execution | 19 | Live persisted-login capability, visible progress, one committed coherent reply | Proof-only; requires an available local Codex login |
| Scheduled / recurring operation | 06 | Trigger creates a real Session and its output is inspectable | Public story; requires live provider |
| Service access lifecycle | 16 | Cookie-free request is 200, revoke is confirmed, the same key becomes 401/403 and cleartext disappears | Proof-only security gate |

## Public marketing stories

| Chapter | Primary product responsibility | Independent readback |
| --- | --- | --- |
| 00 | Stable managed-work contract | Published objects and committed control Event |
| 02 | Agent authoring | Exact publication and live Sandbox result |
| 03 | Official Anthropic SDK integration | SDK-created Session, accepted Event receipt, and the same committed result in Console |
| 04 | Human-controlled repository action | Pinned source read, explicit write approval, and downloadable review artifact |
| 05 | Restart recovery | New release process incarnation, same pending Session and receipt, one approved completion |
| 06 | Deployment | Triggered Session and inspectable output |

## Proof-only tests

These checks are release gates, not video chapters. They return to the public series only
when a consequential task, visible result, human control point, and complete handoff make the
capability meaningful to a first-time viewer.

| Proof | Contract under test | Independent readback |
| --- | --- | --- |
| 01 | Model supply | Verified Provider Connection, imported offering, and real response |
| 03 | Tool governance | Runtime denial before execution |
| 04 | Memory and evidence | Fresh-Session recall plus persisted Memory content |
| 05 | AI-assisted authoring | Validated Draft with explicit human publication authority |
| 06 | State Machine | Runtime refusal reason and absent unsafe effect |
| 07 | All-in-one startup | Captured clean-directory receipt plus embedded Console on the same listener |
| 10 | Skill behavior | Bound Skill activates and changes the real model result |
| 11 | Resource provenance | Realization receipt plus Session Inputs |
| 13 | Managed API | Same Session over public wire and Console |
| 14 | Session lifecycle | Interrupt receipt, archive state, rejected later write |
| 15 | A2A inbound discovery | Console JSON equals public well-known Agent Card |
| 16 | Self-managed service access | Same bearer allowed, revoked, then denied |
| 17 | Frontend protocols | AI SDK and AG-UI commit to one history |
| 18 | MCP server export | Authenticated initialize and explicit tool catalog |
| 19 | Codex ACP | Observed live persisted-login capability and committed complete reply |
| 20 | Tool presentation | Native and deferred MCP aliases preserve canonical tool identity |

Dashboard, Eval, Datasets, Audit, self-hosted placement, and data-subject erasure
remain outside the marketing series until each has a real UI operation and an
independent effect readback. Webhooks now has a complete authoring, secret-handoff,
pause/recovery, failure-state, and deletion UI; it remains a documentation/E2E
chapter until a marketing story also captures a real signed receiver delivery and
the receiver's useful downstream result. This is an implementation boundary, not
missing copy.
