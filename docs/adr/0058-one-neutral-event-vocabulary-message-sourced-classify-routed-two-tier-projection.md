# ADR-0058: One Neutral Event Vocabulary (`journal::AgentEvent`) — Message-Sourced Truth, a Single `classify()` That Routes to Transport Channels, Two-Tier Protocol Projection (Lifecycle Exhaustive / Detail Opt-In), and Engine-Owned Increment Normalization

- Status: Proposed
- Date: 2026-07-16
- Builds on: the message transcript as content truth + the audit event log that is
  explicitly *not* truth (`awaken-store-schema`, `{prefix}_message` vs
  `{prefix}_event`); the commit fence / phase authority (`{prefix}_commit`, G31/G32);
  resume rebuilding the transcript from committed messages (`store::thread_reader`,
  G1/G13); the neutral projection fold + `Transcoder` seam (`project::AgentEvent`,
  `project_messages`/`project_step`/`project_history`); the live stream sink
  (`stream::event::Kind`, `stream::sink::Sink`, `ChannelStreamSink`); the durable
  audit event (`event::RunEvent` → `Draft`); telemetry content-capture consent
  (ADR-0050); the best-effort-live / commit-is-truth discipline (G10/G13).
- Reference architectures (not code dependencies): `~/Codes/goal` — a single stream
  vocabulary with `TextDelta` + `ToolCallReady` splitting the ambiguous tool-argument
  field; `~/Codes/awaken-next` — one flat `AgentEvent` (22 variants), a single
  `EventSink::emit`, a single `classify()` (`event_tier.rs`), a `RuntimeEventDurability`
  fidelity dial, and a two-tier `EventTier` (Lifecycle exhaustive / StreamDetail opt-in,
  ADR-0072 there). This ADR takes their *mechanisms* while keeping this repo's stronger,
  different invariant: **truth is the message log, not an event store.**

## Context

The neutral event surface is split across **three parallel enums**, and the split leaks:

1. **Three producer vocabularies, overlapping.** `stream::event::Kind` (7 live-delta
   variants), `project::AgentEvent` (8 committed whole-unit variants), and
   `event::RunEvent` (5 audit-lifecycle variants) each redeclare
   `RunStarted`/`Waiting`/`RunFinished`/`RunFailed`. Every protocol carries **two**
   transcoders — a live one reading `stream::Kind` and a committed one reading
   `AgentEvent` — kept consistent only by hand (Managed reconciles `evt_N` ids between
   its preview and its committed events).

2. **The tool-argument field lies (leaky abstraction).** `stream::Kind::ToolCall`
   carries `arguments: Value` with *two* shapes: genai streams a **cumulative
   `Value::String`** (`provider-genai/lib.rs`), the non-streaming default pushes a
   **parsed object** (`llm.rs`). Consequence: AI-SDK and AG-UI each re-implement suffix
   de-accumulation (`sent_len`/`is_char_boundary`), and a non-streaming provider's tool
   arguments **silently never stream** on AI-SDK (`as_str()` returns `None`).

3. **The fan-out tax.** Any protocol change forces N×M edits: a flat event enum matched
   exhaustively in every encoder means adding one variant touches all five encoders even
   when only one renders it. `goal` collapses this into one `Transcoder`, but a single
   exhaustive match re-incurs the tax as increment variants grow.

The **key divergence** to get right: `awaken-next` is **event-sourced** — its
`AgentEvent` *is* a canonical store staged atomically with the checkpoint. **This repo is
message-sourced** — the schema says so verbatim (`{prefix}_event` is "NOT the
message-truth source"), and resume rebuilds from `{prefix}_message`. We therefore adopt
awaken-next's *routing/projection mechanisms* but **must not** introduce a canonical
AgentEvent store; that would flip the truth model and break resume/CommitCoordinator.

## Decision

One neutral **read/emit** vocabulary, a single classifier that routes it to real
transport channels, two projection tiers, and increment normalization pushed to a single
upstream owner — while the **write truth stays the message log**. Twelve axes, each an
explicit decision.

### Axis 1 — Truth model: message-sourced (NOT event-sourced). *The load-bearing correction.*

- **Decision.** `{prefix}_message` (+ `{prefix}_state_command` + `{prefix}_commit`)
  remain the sole canonical truth. `AgentEvent` is a **read/emit projection**, never a
  stored source of truth. `{prefix}_event` stays an **audit projection**. We do **not**
  add a canonical AgentEvent store.
- **Why.** Preserves G1/G13 (resume folds committed messages), the commit fence (G31/G32),
  and the existing `CommitCoordinator` atomicity — all of which key on messages. It also
  bounds this refactor to the read/emit side: **no data migration of truth.**
- **Consequence.** awaken-next's "canonical EventStore staged atomically with the
  checkpoint" maps here onto the **existing message commit** (already atomic with the
  checkpoint). AgentEvents for a query are **folded from messages on demand**, not read
  from an event store.

### Axis 2 — Producer vocabulary: one `journal::AgentEvent`.

- **Decision.** Merge `stream::Kind` + `project::AgentEvent` + the *content-lifecycle*
  half of `RunEvent` into one enum, `journal::AgentEvent`. Both producers (the live
  **stream** and the **fold**) emit this one type.
- **Shape** (two nested tiers — see Axis 3):

```rust
// crates/contract/awaken-agent-contract/src/journal/event.rs
pub enum AgentEvent {
    Lifecycle(Lifecycle),  // whole-units + run lifecycle — authoritative, from fold
    Detail(Detail),        // fine-grained increments — best-effort, from stream
}

pub enum Lifecycle {
    RunStarted,
    StepStart,                                   // multi-step tool loop boundary (was a RunStarted hack)
    StepEnd { usage: Option<TokenUsage> },       // usage rides the terminus — no `InferenceComplete` variant
    AssistantMessage { id: String, content: Vec<ContentBlock> }, // content MAY now hold ContentBlock::Thinking
    ToolCall   { id: String, name: String, input: Value, disposition: ToolDisposition },
    ToolResult { id: String, content: Vec<ContentBlock>, is_error: bool },
    Waiting      { pending_tool_use_id: Option<String> }, // = HITL: projects agent.tool_use{ask}
    Continuation { steered: bool, detail: Value },
    RunFinished  { exhausted: bool, usage: Option<TokenUsage> },
    RunFailed    { code: String, message: String },
}

pub enum Detail {
    TextDelta      { delta: String },
    ReasoningDelta { delta: String },            // ★ the hard gap — model reasoning, best-effort increment
    ToolCallDelta  { id: String, name: String, args_delta: String }, // suffix, already de-accumulated
}
```

**Coverage boundary (what belongs vs. what does not).** `AgentEvent` covers exactly
**agent-produced content**: message / tool / **reasoning** / usage. It deliberately does
**not** cover **session/orchestration lifecycle** — `session.status_running/terminated/updated`,
the subagent five, `span.outcome_evaluation_*`, `agent.thread_context_compacted` — those are
minted by the **state layer from a `TurnOutcome`**, are not "what the agent emitted token by
token," and belong to a separate `DomainEvent`/state vocabulary. Forcing them into
`AgentEvent` would be the mistake. **HITL/permission is already covered**:
`Waiting{pending_tool_use_id}` + `ToolDisposition::PendingBuiltin` projects
`agent.tool_use{evaluated_permission:"ask"}`, answered by inbound `user.tool_confirmation`.

**The reasoning gap is double.** Today neither the increment (no `ReasoningDelta`) nor the
committed form (`ContentBlock` has only `Text/Image/ToolUse/ToolResult`, `content.rs:17`)
exists. Both reference architectures (goal, awaken-next) carry reasoning. So this ADR also
adds — **Axis 10b** — a committed `ContentBlock::Thinking { text, signature: Option<String> }`
so `AssistantMessage.content` can carry reasoning as durable truth (folded from messages like
any block). `ReasoningEncryptedValue` (Anthropic native encrypted reasoning passthrough) is
**deferred** — pair it with `ReasoningDelta` only if native encrypted reasoning is run.
This gap is judged against "covers agent-produced content," **not** "covers one protocol's
wire" — Managed itself does not preview thinking today (`session.rs:586`), but AI-SDK/ACP do,
and it is real model output.

- **Name.** Keep `AgentEvent` (do **not** rename to `AgentFact`): it is already a neutral,
  protocol-word-free, widely consumed type; renaming for aesthetics is not worth the churn
  ([[state-reason-before-rename]]). Module `project` → **`journal`**; `project_*` fold
  functions → **`fold_*`**; derived read models → **view**.

### Axis 3 — Two tiers: Lifecycle (exhaustive) / Detail (opt-in). *Kills the fan-out tax.*

- **Decision.** `Lifecycle` is the **semantic** tier — the compiler forces every protocol
  to handle every variant (adding a lifecycle fact *should* make all protocols take a
  stance). `Detail` is the **increment** tier — default no-op; a protocol opts in to only
  the increments it renders. Adding a `Detail` variant touches `classify()` + the one or
  two protocols that opted in, not all five.
- **Why.** Directly from awaken-next's `EventTier` (ADR-0072 there): safety (exhaustive
  match) stays where it matters (semantics); opt-in stays where fan-out hurts (increments).
  Prevents both the ACP-encoder "13 no-op arms" noise and the ADR-0058-there
  `strip_prefix("agent_run_")` semantic-encoding hack.

### Axis 4 — Single `classify()`: the one routing truth.

- **Decision.** One pure function decides, per variant, its tier, its transport channels,
  and its fidelity class. A conformance test asserts it total and consistent. It replaces
  every hand-written mapping scattered across producers/encoders.

```rust
// crates/contract/awaken-agent-contract/src/journal/classify.rs
pub struct Routing { pub tier: Tier, pub live: bool, pub audit: bool, pub fidelity: Fidelity }
pub fn classify(e: &AgentEvent) -> Routing { /* the single source of routing truth */ }
```

- **Why.** awaken-next's `event_tier.rs::classify` proves this removes the "three
  vocabularies drift" problem structurally: routing is one function under one test, not
  N×M implicit correspondences.

### Axis 5 — Channels this repo keeps (and the one it does not).

- **Decision.** `classify()` routes `AgentEvent` to exactly these, per the *transport
  contract* each satisfies — **not** by content:

| Channel | Contract | This repo's mapping |
|---|---|---|
| **live** | best-effort, lossy, high-freq, pre-commit | broadcast of `Detail` (+ `RunStarted`); ephemeral (G10/G13). Managed preview / AI-SDK+AG-UI prefix. |
| **canonical truth** | authoritative, atomic-with-checkpoint | **the message commit** (unchanged, `CommitCoordinator`). *Not an AgentEvent store.* Query = `fold_*` from messages. |
| **audit** | durable, not-truth | `Lifecycle` + audit facts → `Draft` → `{prefix}_event`. |
| **permission** | **synchronous** request/response | stays out of `AgentEvent` entirely (ACP `request_permission` bidirectional); only the *fact* `PermissionDecided` projects to audit. |

- **Explicitly deferred (YAGNI unless required):** a **ProtocolReplayLog** (byte-identical
  reconnect across encoder upgrades) and an **Outbox** beyond what already exists
  (webhooks/live-inbox). This repo reconnects by **re-folding messages**; adopt a ReplayLog
  only if byte-identical replay becomes a hard requirement.

### Axis 6 — Producer authority: stream ⟶ Detail only; fold ⟶ Lifecycle only. *Invariant.*

- **Decision.** The live **stream** producer emits **only** `Detail` increments (plus the
  opening `RunStarted`). The **fold** producer emits **only** `Lifecycle` whole-units and
  run lifecycle. No variant has two producers.
- **Why.** This is what makes one enum with two authority levels type-sound: because
  `AssistantMessage` can only come from the fold and `TextDelta` only from the stream, a
  consumer never has to ask "is this `RunFinished` best-effort or authoritative?" It also
  **strengthens** G10/G13: the live stream now *structurally cannot* carry an
  authoritative terminus. (Today `stream::Kind::RunFinished` exists and transcoders ignore
  it for the real finish — that best-effort lifecycle emission is deleted.)

### Axis 7 — Fidelity dial (optional here, coupled to ReplayLog).

- **Decision.** Model the dial `RuntimeEventDurability::{Off, Compacted, Full}` but note it
  is **only meaningful if a ReplayLog is added** (Axis 5). Content is durable via messages
  regardless of the dial; the dial governs whether *increments* are captured for
  replay/telemetry (`Compacted` drops `Detail`, `Full` keeps them). Until a ReplayLog
  exists, `Detail` is live-only-ephemeral and the dial is a no-op — do not build it
  speculatively.

### Axis 8 — Single fan-out point.

- **Decision.** One `EventSink::emit(event)` per source: (1) publish to **live** (infallible,
  best-effort) → (2) `classify()` → (3) route the authoritative side. In this repo step 3
  is the **existing message/commit path** for content+lifecycle and the **audit Draft** for
  audit facts — *not* a second durable AgentEvent write. One source, one call, fan-out by
  contract.

### Axis 9 — Two-tier `Transcoder`; ids stay per-protocol.

- **Decision.** One transcoder per protocol (deleting the second, committed one). It
  implements an **exhaustive** `lifecycle()` and an **opt-in** `detail()`:

```rust
pub trait Transcoder {
    type Wire;
    fn lifecycle(&mut self, e: &Lifecycle) -> Vec<Self::Wire>;      // compiler-exhaustive
    fn detail(&mut self, _e: &Detail) -> Vec<Self::Wire> { vec![] } // default no-op, opt-in
}
```

- Because one transcoder now sees both `Detail::TextDelta` (preview) and the later
  `Lifecycle::AssistantMessage` (committed) with a single `open_text`, **Managed's
  cross-transcoder `evt_N` reconciliation disappears** — one instance, one id.
- **Boundary (do not violate).** Wire-scoped id minting stays **per protocol**: Managed's
  `event_seq` + `append_turn` reuse, AI-SDK `txt-N`, AG-UI `{run_id}-msg-N`. `classify()`
  and the segmenter-free tiers never mint wire ids. Semantic ids (tool-call id) stay
  runtime-stable.

### Axis 10 — Tool arguments: type-split + engine-owned de-accumulation. *Do this first.*

- **Decision.** Kill the leaky `arguments: Value`:
  - `Detail::ToolCallDelta { args_delta: String }` — always a **suffix fragment**.
  - `Lifecycle::ToolCall { input: Value }` — the parsed object at the completion point.
  - `DeltaSink` splits into `on_tool_call_delta(id, name, args_delta: &str)` and the
    committed `ToolCall` comes from the fold.
- **Owner.** The **provider adapter** de-accumulates (it is genai's private quirk; keep it
  behind the anti-corruption boundary, not in the engine `StreamDeltaSink`, which is `&dyn`
  and per-run only). The non-streaming default emits one whole-string `ToolCallDelta` +
  the parsed `ToolCall` — same shape as streaming. This alone removes the three
  `sent_len` copies **and** the "non-streaming provider silently doesn't stream args" bug.

### Axis 11 — Module moves.

| Before | After | Action |
|---|---|---|
| `stream/event.rs` (`Kind`) | — | **delete**; folded into `journal::AgentEvent` |
| `stream/sink.rs` (`Sink`) | `journal/sink.rs` | move; payload `Kind` → `AgentEvent` |
| `project.rs` (`AgentEvent`, `Transcoder`, `project_*`) | `journal/{event,tier,classify,transcoder,fold}.rs` | rename/extend; `project_*` → `fold_*` |
| `event/run_event.rs` (`RunEvent`) | — | content-lifecycle variants **merge** into `AgentEvent`; the type as a *third vocabulary* disappears |
| `event/{kind,draft,record}.rs` | `audit/{kind,draft,record}.rs` | **keep**: audit's durable envelope (a *view*, not truth) |
| `agent/{message,content,run,state,...}` | unchanged | domain value objects `AgentEvent` references |
| `fact/`, `commit/coordinator.rs` | unchanged | `commit/staged.rs`: `assemble` shifts from hand-built events to `fold_step` view |
| `state/types.rs` `TurnOutcome`, `state/events.rs` `append_turn`, `project.rs` `project_turn` | `StepOutcome`, `append_step`, `fold_step` | **rename turn → step** ([[turn/run/step vocabulary]]): the runtime port is already `run()`/`resume()`, and `awaken-protocol-transport` already returns `StepOutcome` — Managed is the lone `Turn*` holdout. `turn` survives only in `types/session.rs` doc comments as a Managed-Agents **wire-spec** mirror; the port and all execution vocabulary is run/step. |

### Axis 12 — Migration sequence (each step compiles + tests green; standalone commits).

0. **Tool-arg type split + de-accumulation** (Axis 10). Smallest, root-cause, independently
   fixes the silent-non-streaming bug. Prereq for the increment tier.
1. **Introduce `journal::AgentEvent` two-tier enum + `classify()` + conformance test**
   (Axes 2–4). Keep old paths compiling via `From`/shims.
2. **Producer authority split** (Axis 6): stream emits only `Detail`(+`RunStarted`); fold
   emits `Lifecycle`. Delete best-effort lifecycle from the stream.
3. **Collapse each protocol to one two-tier `Transcoder`** (Axis 9); delete the live/committed
   twin and Managed's cross-transcoder id reconciliation.
4. **Merge `RunEvent` content-lifecycle; move `event/` → `audit/` as a projection** (Axes 5,
   11). Audit stays a `fold`-derived Draft; audit-only facts do not enter the protocol tiers.
5. **Rename `project` → `journal`, `project_*` → `fold_*`** (Axis 11). Mechanical.

Withdraw the in-flight `LiveBounder`/`LiveEvent` (a sideways segmenter): this ADR supersedes
it — the boundary decision moves into the tiers + producer-authority split, not a shared
segmenter. Keep the thinking channel, re-expressed as `Detail::ThinkingDelta`.

## Invariants preserved

- **G1/G13** — resume/query fold committed messages; `AgentEvent` never becomes stored truth
  (Axis 1).
- **G10/G13** — live is best-effort; authoritative terminus only from the fold — now
  *structurally* enforced (Axis 6).
- **G31/G32** — commit fence / phase authority untouched (Axis 1).
- **ISP / bounded context** — protocol transcoders never match audit-only facts; audit is a
  separate projection (Axes 5, 11).
- **No credential/secret in the runtime surface** — unchanged; `AgentEvent` carries content
  and lifecycle, never credentials.

## Consequences

- **Positive.** Three neutral vocabularies → one; two transcoders per protocol → one; three
  `sent_len` copies → zero; routing correspondence hand-written N×M → one tested `classify()`;
  the silent-non-streaming-args bug fixed by type; Managed id reconciliation gone; adding an
  increment costs `classify()` + one renderer, not five.
- **Negative / risk.** A single `AgentEvent` mixes two authority levels — sound **only**
  while Axis 6 holds; the conformance test for `classify()` and a producer-authority test are
  load-bearing. The migration is broad (contract + runtime + all protocols + `commit/staged`);
  it must land as its own sequence (steps 0–5), not folded into unrelated work.
- **Deferred.** ProtocolReplayLog + fidelity dial (Axes 5, 7) — build only if byte-identical
  reconnect is required.
