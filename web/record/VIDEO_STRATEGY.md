# Awaken video architecture

The series is organized by user outcome, not by sidebar destination. Every recording
starts with intent and capability, proves one claim against the real UI/API, and ends
with one contrast that can stand alone as a shareable clip.

## Dynamic view — the audience journey

```text
discover       first value         differentiated control       hard proof          trust
   │               │                         │                       │                 │
   ▼               ▼                         ▼                       ▼                 ▼
overview → connect model → build one agent → control its runtime → violate a rule → inspect evidence
                                │                   │                    │
                                └──── protocols / sandbox / MCP ────────┘
```

The opening establishes the promise in under a minute. The next video gets a user to
a live answer. Later videos deepen confidence with configuration, runtime enforcement,
protocol portability, and operational evidence. This order optimizes time-to-value
before asking the viewer to learn the architecture.

## Static view — value chapters and source UI

| Chapter | User question | UI grouped into the story | Aha moment |
|---|---|---|---|
| 00 · Overview | Why Awaken? | Home, Agents, Sessions, Models, environments | One control plane from configuration to evidence |
| 01 · First success | Can my model work now? | Models, Credentials, live test | A key becomes a verified executor, not a hidden setting |
| 02 · Build | Can I create a useful specialist? | Agent Overview, publish diff, Sandbox | One config becomes a versioned live agent |
| 03 · Govern tools | Can it act without becoming unsafe? | Tools, permission rules, approval | Tool identity and policy are inspectable data |
| 04 · Grounding | What knowledge does it use? | Resources, Skills, Memory, session trace | Inputs and evidence stay visible |
| 05 · AI authoring | Can the model configure the platform? | Assistant, capability contract, config diff | AI proposes only capabilities the runtime advertises |
| 06 · Runtime proof | Are constraints real? | State Machine, live read-before-write violation | The unsafe write is stopped before the tool executes |
| 07 · Isolation | Where and how does it run? | Environments, Native/ACP, sandbox | Protocol, placement, and containment are replaceable config |
| 08 · Agent control plane | Can every behavior be tuned per Agent? | context, compaction, Memory prompts, reminders, continuation | What the model sees, remembers, and must finish lives together |
| 09 · Protocol composition | Can I use Managed Agents and MCP directly? | Managed session, environment, ACP, inline MCP, Vault hint | Protocol choices compose at the boundary without changing the Agent |

Dashboard, Eval, Datasets, Audit, Access, Vaults, Deployments, and Settings remain in
the UI smoke inventory. They should enter a video only when their backend capability
is enabled and the story can prove a result; a gated or empty page is not a product
payoff.

## Copy and interaction rules

- Subtitle one thought at a time; lead with the outcome, then name the mechanism.
- Prefer concrete verbs: “blocks before execution”, “recalls bounded context”,
  “persists across this thread”. Avoid architecture nouns without an observable effect.
- Keep code/config snippets to the smallest decisive fragment and visually pair them
  with the resulting UI state or runtime event.
- Never let a spinner carry the story. Show an immediate queued/running state, then a
  success result or an actionable error. Quota/auth failures must say what the user can
  do next.
- Keep the product chrome recognizable, use one brand close, and cut each Aha into a
  6–12 second standalone clip for sharing.

## Installation claim

The current `awaken` binary serves the aggregated backend but does not embed the built
console. Until packaging serves both resources from one command, the installation
story must show the binary plus the console server and must not claim that starting one
binary is sufficient. A one-command installation video becomes release-ready only when
its checkpoint opens the console from a fresh install without a separate Vite process.
