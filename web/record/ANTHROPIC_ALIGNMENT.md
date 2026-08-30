# Anthropic Managed Agents alignment review

Reviewed against Anthropic's current official sources on 2026-08-11:

- [Claude Managed Agents overview](https://platform.claude.com/docs/en/managed-agents/overview)
- [Scaling Managed Agents: Decoupling the brain from the hands](https://www.anthropic.com/engineering/managed-agents)
- [How we contain Claude across products](https://www.anthropic.com/engineering/how-we-contain-claude)

## Vocabulary boundary

Anthropic's public product contract names four core concepts: **Agent, Environment,
Session, and Events**. Awaken's opening chapter uses that exact audience-level sequence.
Control, Coordinator, Worker, publication, placement, and realization remain Awaken
implementation/operating concepts; marketing may show them when the product UI or a
runtime receipt proves them, but must not attribute those names to Anthropic.

| Anthropic public concept | Awaken visible contract | Video owner | Claim rule |
| --- | --- | --- | --- |
| Agent | Versioned Agent draft/publication with model, instructions, tools, MCP, Skills, and resources | Stories 00, 02; proofs 03 through 06, 10 | A Draft is not live until publication readback succeeds |
| Environment | Placement and Sandbox configuration selected by a Session | Stories 00, 12; proofs 07, 19 | Isolation is claimed only when the Worker manifest/runtime observation proves it |
| Session | Durable unit of task execution and history | Stories 00, 12; proofs 04, 11, 13, 14, 17, 19 | UI and public-wire IDs must refer to the same Session |
| Events | Committed messages, tool/lifecycle results, status, and control receipts | Stories 00, 12; proofs 04, 13, 14, 17 | A toast or transient stream frame is not durable evidence |

## Architecture and security themes

Anthropic's engineering article separates the durable session log from the replaceable
harness (“brain”) and execution/tool boundary (“hands”). Awaken's corresponding
marketing story is deliberately expressed as stable interfaces rather than as a claim
of identical implementation:

- the Session Event log is the durable readback authority;
- the Agent harness/adapter can change without changing the Session identity;
- execution is selected through an Environment and observable Worker capability;
- Resource and Vault credentials are resolved outside generated-code presentation;
- interruption, archive, revocation, and denied writes are shown as fail-closed effects.

The containment article reinforces blast-radius control and tenant separation. Awaken
therefore does not use configuration screenshots as isolation proof: read-only Resource
mounts require an enforcing Sandbox manifest, and the Codex ACP claim requires an
observed non-root container plus a committed response.

## Deliberate differences and non-claims

- Awaken is not presented as Anthropic's hosted service or as wire-compatible by brand
  association. Proof 13 verifies Awaken's Managed Agents-style public
  session contract and beta header rather than claiming product identity.
- Anthropic documents managed cloud and self-hosted sandboxes. Awaken shows only the
  placement actually available to the recording host; unavailable self-hosted/Docker
  evidence remains blocked.
- Anthropic documents scheduled deployments, Skills, MCP, persistent history, and
  steering/interrupt. Awaken uses a public story only when the mechanism produces a
  customer result. Setup and compatibility remain proof-only.
- Beta/research-preview, retention, certification, pricing, SLA, and commercial terms
  are never inferred from source code or from Anthropic's product status.

## Copy decision

The series opener stays centered on **Agent → Environment → Session → Event**. The
marketing line is "the harness evolves while stable work interfaces preserve
operability," not "Awaken reproduces Anthropic." Specialized chapters then prove
resources, scheduling, protocols, access, and runtime containment through their own
observable effects.
