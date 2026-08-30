# End-to-end user journey and 100-point UX audit

This checklist records the repository-owned release criteria and evidence. External
product research is maintained outside the public repository; a prior `PASS` must be
re-earned against the current build.

This document is a release gate, not a feature inventory. A capability is considered
complete only when a person can understand the intent, perform the operation, observe
the committed effect, and recover from failure without reading an opaque identifier.

Evidence labels: **B** = real-browser review, **A** = automated contract/test,
**R** = release runtime/API readback, **S** = source responsibility trace. `PASS` means
the current release candidate has that evidence; `GATED` means the product truthfully
hides or labels a capability that the running deployment does not expose.

## Complete journey

| Stage | Human goal and primary UI | Backend owner/effect | Closure evidence |
| --- | --- | --- | --- |
| 1. Enter | Open Console, understand product and active tenant | suite navigation, local/cloud session, workspace context | readable Workspace, organization, and user; expired auth returns to sign-in |
| 2. Get ready | Overview readiness and next-best action | capabilities, executable models, Agent publications, Environments | each failed prerequisite links to its owning page |
| 3. Connect | Models & providers, Credentials | credential vault, provider connection, discovery, publication resolver | secret-free verified connection plus a real model test |
| 4. Build | Agent Quickstart and advanced Build sections | versioned config draft, validation, publication compiler | reviewed diff and published version readback |
| 5. Add knowledge | Skills, Files, Memory, MCP and Vault bindings | resource stores, version stores, realization manifest, secret materializer | bound version and realized Session Inputs agree |
| 6. Choose execution | Environments and Sandbox policy | environment registry, package/image plan, networking, placement | execution receipt identifies the selected Environment and boundary |
| 7. Run | Sessions and inline Agent test | managed Session application, Coordinator, Worker, model/ACP adapter | durable user and agent Events on the same Session |
| 8. Control | steer, approve/deny, interrupt, archive | event admission, permission evaluator, terminal lifecycle | committed control receipt; rejected unsafe or post-terminal work |
| 9. Observe | conversation, inputs, artifacts, integrations, trace, usage | event log, resource provenance, artifact store, observability | every displayed result links back to its Session and version |
| 10. Operate | Deployments, protocols, A2A, MCP export, Access | scheduler, Managed/AI SDK/AG-UI/A2A/MCP adapters, IAM | manual trigger/readback; token mint/revoke/deny; protocol points to same runtime |
| 11. Govern | Runtime secrets, least privilege, lifecycle notifications | Vault, token roles, signed Webhooks, user profiles, consent/erasure | secrets never echo; Webhook failures remain visible; destructive effects have durable receipts |
| 12. Recover | refresh, retry, resume, archive and failure diagnosis | durable stores, recovery scan, work lease/requeue, auth transition | refresh is safe; expired work is requeued; actionable failure state remains visible |

Anthropic's public Managed Agents vocabulary is used only for the audience-level
Agent → Environment → Session → Events journey. Awaken-specific Control,
Coordinator, Worker, publication, placement, and realization remain implementation
responsibilities and are not attributed to Anthropic.

## Coverage boundary

The main Console covers Agent authoring, Skills, Files, Memory/Dreams, Sessions,
Deployments, Artifacts, Environments, Models/providers, MCP overview, integration
protocols, signed Webhooks, A2A, Access, Vaults, Settings, and the Console Assistant. User Profiles,
consent/erasure, cloud Tunnels, aggregate audit, datasets, and Eval APIs are
backend or deployment-gated capabilities today. They must not be marketed as complete
Console journeys until a dedicated UI provides intent → operation → effect → readback.
The gated routes state this boundary instead of presenting a dead or simulated UI.

## 100 review points

### Entry, identity, tenancy and recovery

| # | Check | Status/evidence |
| ---: | --- | --- |
| 001 | Expired authentication clears cached product bearer | PASS A |
| 002 | Any API 401 triggers one global sign-in transition | PASS A |
| 003 | Refresh with an invalid session shows sign-in, not a raw error | PASS A/R |
| 004 | Setup-token failure explains invalid, expired, and used states | PASS S |
| 005 | Connection failure offers a bounded retry | PASS S |
| 006 | Workspace primary label never exposes a long coordinate | PASS A/B |
| 007 | Generated local Workspace renders as “Local Workspace” | PASS A/R |
| 008 | Personal hosted scope renders as “Personal Workspace” | PASS A |
| 009 | Organization is a readable name, not `org_*` | PASS R |
| 010 | Current user is readable and account details are discoverable | PASS B/R |

### Information architecture and navigation

| # | Check | Status/evidence |
| ---: | --- | --- |
| 011 | Navigation follows Control → Build → Run → Connect → Govern | PASS B/S |
| 012 | Resource pages are grouped under Build without a duplicate route | PASS B/S |
| 013 | Mobile replaces the dense sidebar with one grouped page selector | PASS B |
| 014 | Current mobile page is selected after navigation | PASS B |
| 015 | Search and sidebar share human-readable group names | PASS B/S |
| 016 | Search matches English name, Chinese name, and workflow group | PASS A/S |
| 017 | Empty search result explains what to try next | PASS B/S |
| 018 | Every top-level route has one H1 and purpose statement | PASS A/B |
| 019 | Every purpose statement includes a recommended next action | PASS A/S |
| 020 | Unknown routes return to Overview rather than a blank shell | PASS S |

### Visual hierarchy, color and typography

| # | Check | Status/evidence |
| ---: | --- | --- |
| 021 | Body copy is at least 14px and metadata at least 12px | PASS A |
| 022 | Faint color is reserved for metadata, not required instructions | PASS A |
| 023 | Dark theme is the branded default and persists | PASS A/B |
| 024 | Light theme updates native control color scheme | PASS A/B |
| 025 | Topbar SVGs are explicitly bounded to 16×16 | PASS A/B |
| 026 | Accent, success, warning and danger are used semantically | PASS S/B |
| 027 | Cards use consistent surface, border, radius and shadow tokens | PASS S |
| 028 | Monospace is limited to identifiers, code, and technical evidence | PASS S |
| 029 | Remote fonts cannot delay or disclose production page loads | PASS A |
| 030 | Focus-visible styles remain visible in both themes | PASS B/S |

### Responsive layout and touch

| # | Check | Status/evidence |
| ---: | --- | --- |
| 031 | All 17 top-level pages fit 390px without page overflow | PASS B |
| 032 | All 17 top-level pages fit 768px without page overflow | PASS B |
| 033 | All 17 top-level pages fit 1280px without page overflow | PASS B |
| 034 | Wide tables scroll inside their card instead of clipping the page | PASS A/B |
| 035 | Search input can shrink without wrapping labels | PASS A/B |
| 036 | Topbar actions wrap as one deliberate mobile row | PASS B |
| 037 | Mobile chrome actions have 40px touch targets | PASS A |
| 038 | Mobile general actions have at least 36px height | PASS A/B |
| 039 | Inline empty-state links are 32px, and 40px on phones | PASS A/B |
| 040 | Floating Assistant cannot cover the last mobile action | PASS B/S |

### Readiness and model connection

| # | Check | Status/evidence |
| ---: | --- | --- |
| 041 | Overview separates model, Agent publication, and Environment readiness | PASS B |
| 042 | A blocked prerequisite links to the owning configuration page | PASS B |
| 043 | Provider cards use display names as primary labels | PASS B/S |
| 044 | Credential values never return in list/readback UI | PASS A/S |
| 045 | Connect describes credential, endpoint, verification, and discovery | PASS S |
| 046 | Provider verification failure preserves an actionable cause | PASS A/R |
| 047 | Provider retry is bounded and cannot hang recording indefinitely | PASS A/R |
| 048 | Only active executable models appear in Agent setup | PASS A |
| 049 | Model test uses a fixed, reviewable prompt | PASS A/S |
| 050 | Real-provider claims require actual response readback | PASS R/GATED |

### Agent authoring and publication

| # | Check | Status/evidence |
| ---: | --- | --- |
| 051 | Quickstart precedes advanced options | PASS B |
| 052 | Templates explain the behavior they add | PASS B/S |
| 053 | Draft and published state are visually distinct | PASS B |
| 054 | Validation errors identify the exact section | PASS A/S |
| 055 | Review shows intended diff before publication | PASS A/R |
| 056 | Publishing is the only activation authority | PASS A/S |
| 057 | Existing Sessions remain pinned to their reviewed version | PASS A/R |
| 058 | AI Assistant proposes a diff but cannot silently activate it | PASS A/S |
| 059 | Collaboration editing starts from the Coordinator | PASS S |
| 060 | Agent names are primary; IDs remain secondary evidence | PASS B/S |

### Resources, Environments and runtime boundaries

| # | Check | Status/evidence |
| ---: | --- | --- |
| 061 | Files explains upload versus Agent binding | PASS B/S |
| 062 | Artifacts explains output versus reusable input | PASS B/S |
| 063 | Skills distinguishes instruction-only from Sandbox-required | PASS A/B |
| 064 | Memory separates editable Store from Dream output | PASS B/S |
| 065 | Resource version/provenance is inspectable from Session Inputs | PASS R |
| 066 | Environment copy explains packages, network, limits, and timing | PASS B/S |
| 067 | Clearing package configuration sends explicit null semantics | PASS A |
| 068 | Default Environment is sufficient for the simple path | PASS B/S |
| 069 | Vault display names are primary and credential targets secondary | PASS B/S |
| 070 | Isolation claims require a runtime receipt, not configuration copy | PASS R/GATED |

### Session execution, control and observation

| # | Check | Status/evidence |
| ---: | --- | --- |
| 071 | Session creation requires a published ready Agent | PASS A/B |
| 072 | User and Agent Events persist on one Session identity | PASS R |
| 073 | Streaming does not replace durable event readback | PASS A/R |
| 074 | Human approval pauses before the protected effect | PASS A/R |
| 075 | Denial records the reason and omits the unsafe effect | PASS A/R |
| 076 | Interrupt produces a durable control receipt | PASS R |
| 077 | Archive rejects later writes | PASS A/R |
| 078 | Session tabs separate task, inputs, artifacts, integrations and trace | PASS B |
| 079 | Child threads remain attributable to the owning Agent/version | PASS A/S |
| 080 | Usage is shown as evidence, not confused with task progress | PASS B/S |

### Operation, integrations and governance

| # | Check | Status/evidence |
| ---: | --- | --- |
| 081 | Deployment requires a published Agent and explicit schedule | PASS A/B |
| 082 | “Run once” creates an independently inspectable Session | PASS R |
| 083 | Pause/unpause/archive have distinct lifecycle meaning | PASS A/S |
| 084 | Protocol overview chooses client type before connection steps | PASS B/S |
| 085 | Each protocol owns its own credential and connection help | PASS A/B |
| 086 | AI SDK and AG-UI use short-lived application tokens | PASS A/R |
| 087 | A2A Card exposes the same published runtime truth | PASS R |
| 088 | MCP export requires dedicated authentication | PASS R |
| 089 | Service keys default to least privilege and show once | PASS A/B |
| 090 | Revoked key is rejected on the same protected operation | PASS R |

### Reliability, copy and release-video truth

| # | Check | Status/evidence |
| ---: | --- | --- |
| 091 | Work owner loss requeues rather than strands work | PASS A/R |
| 092 | Expired Worker incarnation cannot revive by heartbeat | PASS A |
| 093 | Session recovery scans preserve retryable and quarantined distinctions | PASS A/R |
| 094 | Every page empty state supplies meaning and a next step | PASS B/S |
| 095 | Destructive confirmations name the irreversible effect | PASS B/S |
| 096 | English and Chinese copy express the same responsibility boundary | PASS A/S |
| 097 | Frontend build precedes embedding in the release binary | PASS R |
| 098 | Recording commands and network waits have hard timeouts | PASS A/S |
| 099 | A publishable video proves intent → operation → effect → readback | PASS A/R |
| 100 | Missing Docker/provider/deployment capability produces a diagnostic, never a stale MP4 | PASS A/R/GATED |

## Release decision rule

Any later regression changes the corresponding `PASS` to `OPEN`, records the root
cause and affected sibling surfaces, adds a causal/decision-table test, and reruns the
browser sizes and relevant closed-loop video chapter. A successful build alone cannot
promote an `OPEN` item.
