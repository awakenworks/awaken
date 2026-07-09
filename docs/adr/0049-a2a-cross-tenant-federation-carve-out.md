# ADR-0049: A2A is Cross-Tenant Federation — a Carve-Out from the Front-Door Envelope

- Status: Accepted
- Date: 2026-07-10
- Carves out from: [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md)
  D3 ("one envelope, three vocabularies — no front door invents its own tenancy,
  auth, or routing")
- Fulfils: [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)
  D8 (which deferred the A2A decision to its own ADR)
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (protocol
  is a projection over the neutral core), G16 (only front-door adapters name
  product vocabulary)

## Context

ADR-0042 D3 fixes one envelope for the data-plane front doors — Managed Agents,
AI-SDK-compatible, AG-UI: **transport HTTP+SSE, auth a workspace-scoped bearer
key, tenancy key→workspace, addressing single-host + globally-unique id in path,
one authorization/approval semantics.** Its rule: *"no front door invents its own
tenancy, auth, or routing."* That rule holds for those three because every caller
holds one of *our* workspace-scoped keys, so tenancy is always the caller's own.

A2A (Agent-to-Agent) breaks that assumption. It is **inter-tenant, inter-provider
federation**: an agent in tenant/provider X calls an agent in tenant Y (possibly a
different deployment). The `awaken-protocol-a2a` adapter already exists as an
*outbound/internal* projection (a coordinator's sub-sessions), but the **inbound
federation** case — a foreign agent calling one of ours — cannot fit the D3
envelope, and pretending it does would be wrong.

## Decision

### D1: Internal multi-agent stays inside the D3 envelope; A2A federation is the carve-out

- **Internal multi-agent orchestration is NOT A2A federation.** A coordinator's
  sub-sessions are **same-tenant**, linked by parent session id as threads, and
  ride the ordinary key→workspace envelope (ADR-0042 D3). Nothing here changes.
- **Inbound A2A federation IS a carve-out.** A call from a foreign agent is a
  different beast and gets its own tenancy, auth, and routing — the three things
  D3 forbids a *data-plane vocabulary adapter* from inventing. A2A is therefore
  **not** classified as a D3 vocabulary adapter; it is a federation surface.

### D2: A2A resolves the callee's tenancy from the URL, not from the credential

The D3 rule "tenancy = the API key's workspace" assumes the caller holds our key.
An inbound A2A caller does **not**. So the **callee's** `workspace` (and `agent`)
are addressed in the URL — the A2A *agent-card* address is a stable, shareable
locator (`{workspace}` + `{agent}`), e.g. `…/a2a/agents/{agent}` under a
workspace-addressing host. This is the one place a URL legitimately carries
tenancy, and only for the *callee*; it does not reopen ADR-0042 D2 for the
key-based data plane (Managed/AI-SDK/AG-UI stay flat + key-resolved).

### D3: The caller authenticates as a foreign principal, authorized by an explicit federation grant

An inbound A2A request is **not** authenticated as one of our `ApiToken`
principals. It presents a foreign identity (a peer deployment's signed assertion /
WIF token). Authorization is an **explicit federation grant** on the callee's
workspace ("workspace W accepts agent A2A calls from principal P for action X"),
evaluated by the same default-deny PDP. Absent a federation grant, an inbound A2A
call is denied — federation is opt-in per workspace, never implicit. Sharing an
agent-card URL grants nothing (mirrors ADR-0042 D2's core property).

### D4: What is deferred

The concrete inbound wiring — the agent-card resolution endpoint, the foreign-
principal verification (peer trust / WIF), the federation-grant shape and its
admin surface, and cross-deployment routing — is deferred to its implementation
slice. This ADR fixes only the **boundary**: A2A federation is a distinct surface
with its own tenancy (callee-in-URL), auth (foreign principal), and routing
(agent-card), carved out of the D3 envelope; internal multi-agent is unaffected.

## Consequences

- ADR-0042 D3 stays true for its three front doors; A2A is not shoehorned into it
  and does not weaken the "one envelope" guarantee for the key-based data plane.
- Federation is opt-in and default-deny: a workspace exposes agents to A2A peers
  only via an explicit grant, so the tenant boundary holds across deployments.
- The existing internal multi-agent path (parent-session threads) is documented
  as *not* federation, preventing it from accidentally adopting A2A's foreign-auth
  semantics.

## Non-Goals / Deferred

- The inbound A2A implementation (agent-card endpoint, foreign-principal
  verification, federation-grant admin surface, cross-deployment routing) — D4.
- Any change to the key-based data plane (ADR-0042 D2) — unamended.

## References

- [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md) — D2
  key-based tenancy, D3 the one-envelope rule this ADR carves out of.
- [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)
  — D8 named this carve-out and deferred it here.
