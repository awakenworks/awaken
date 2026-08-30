# Awaken marketing video standard

The public series shows useful work, not product vocabulary. A first-time viewer should
understand the task, the risk, the result, and the human boundary without knowing
what an Agent, Session, Resource, or Deployment means beforehand.

Every public video follows one line:

```text
consequential task -> real input -> Agent action -> useful deliverable -> human control -> clear handoff
```

The target release set contains six complementary stories. They follow one operating
journey from first decision to repeated, governed work, rather than slicing the product
into settings tours. Technical checks remain in `proofs/`. They protect the claims
without asking a customer to watch setup, protocol handshakes, denial tests, or infrastructure receipts as marketing content.

## What the strongest official examples do

Reviewed on 2026-08-16 against primary sources:

- [Anthropic, Agents for financial services](https://www.anthropic.com/news/finance-agents)
- [Anthropic, Scaling Managed Agents](https://www.anthropic.com/engineering/managed-agents)
- [LangChain, Build a data analysis agent](https://docs.langchain.com/oss/python/deepagents/data-analysis)
- [LangChain, Deep Agents overview](https://docs.langchain.com/oss/python/deepagents/overview)
- [DeerFlow official case studies](https://deerflow.tech/)

Anthropic names familiar jobs and finished work: a pitchbook, a KYC escalation package,
a month-end close report. The examples also say where a person reviews and approves the
work before it reaches a client or is acted on.

LangChain's strongest tutorial begins with a CSV and ends with analysis, visualizations,
and a Slack delivery. Planning, sandbox execution, and tracing explain why the result is
credible, but the result remains the center of the story.

DeerFlow leads with artifacts people can see: a forecast webpage, a generated video, an
explanatory comic, and a Titanic analysis with charts. The visible artifact makes the
Agent's work legible before the viewer studies the harness.

The transferable pattern is work input and useful result first, mechanism second.
Awaken applies it to reproducible technical work: a named evidence file, an auditable
decision, a reviewed Agent revision, an existing SDK client, a protected action, a
restart, and a scheduled repository check. Awaken does
not present these demonstration scenarios as deployed customer cases.

## Public release set

Awaken is a development platform, but the videos do not invent operators to prove that
point. Each video begins with a recognizable technical job. A role appears only where
the product enforces a real responsibility boundary, such as final release approval.

| Video | Status | Task and content | Complete loop | What the viewer learns |
| --- | --- | --- | --- | --- |
| 00 · Evidence to decision | Executable | Mount specified material read-only, inspect the real read trace, and receive a reviewable decision | Exact source → Agent read → HOLD and actions → durable Session → human approval remains required | Awaken delivers traceable work rather than an ungrounded chat answer |
| 02 · Build entirely in UI | Executable | Configure an API compatibility reviewer in Console, Preview it against a breaking contract diff, review the configuration diff, and publish the exact revision | Visible configuration → isolated Preview → real compatibility finding → review → immutable publication for new Sessions | Agent behavior is configurable, testable, and reviewable without editing code |
| 03 · Connect with Anthropic SDK | Executable | Use an existing Managed Agents client to create and continue work | SDK request → Awaken Session → committed result → same Session in Console | A technical team can connect an existing client without adopting a second history model |
| 04 · Human-controlled action | Executable | Investigate a real local repository and prepare a protected change | Repository evidence → Agent plan → permission wait → human decision → inspectable artifact | Automation can reach consequential work without hiding the authority boundary |
| 05 · Survive restart | Executable | Start work, restart Awaken, and continue the same Session | Accepted input → committed progress → service restart → same Session resumes → final result | Durable Session and recovery have a visible operational meaning |
| 06 · Scheduled operation | Executable | Capture a real repository snapshot and schedule an exception-only maintenance brief | Repository facts → read-only snapshot → Deployment → Run once → separate result Session → human next action | A verified Agent can become repeatable work without turning each run into an opaque cron job |

Video 00 is the outcome flagship. Video 02 is the product-control flagship. If a viewer
cannot answer “what file did the Agent read, what decision did it reach, and who still
approves?” after video 00, it does not ship. If a viewer cannot answer “what can I
control, how do I test it, and how do I know the published Agent is the one I reviewed?”
after video 02, it does not ship.

## Video 02: complete UI control without a settings tour

The story is not “Awaken has many fields.” It is “an API compatibility policy can
become a reviewed Agent without editing code or trusting hidden defaults.”

1. Start with the risk: a removed response field and renamed status values must not
   be smoothed into a safe-to-publish decision.
2. Choose the model route and visible execution policy: reasoning level, speed,
   processing geography, and exact fallback identities.
3. Write the job and bound its work: system instructions, step limit, and context policy.
4. Grant only the work inputs and actions it needs: Resources, Memory, Skills, MCP,
   catalog tools, client tools, dynamic tool patterns, permissions, presentation, and
   recovery policy.
5. Define how work proceeds: State Machine, specialist roster, delegation budgets,
   runtime extensions, and metadata.
6. Run the unsaved draft against the real model. The result must identify both client
   breaks and state the migration work required before publication.
7. Review the exact configuration and Resource snapshot, publish it, then read the
   immutable revision back from the API.

The edit is selective. The camera shows one consequential choice from each control
layer, then uses the review diff to prove the whole authored configuration. Long lists,
credentials, and waits do not become footage. Raw JSON remains a lossless inspection
and migration escape hatch, not the primary way a user must configure a supported
Agent capability.

## Cross-video capability coverage

`P` means planned coverage: the story design makes the capability understandable. `E`
means a current executable recording visibly proves the real product behavior. A planned
cell is not evidence. The matching proof test still runs when a mechanism is not narrated.

| Capability | 00 | 02 | 03 | 04 | 05 | 06 |
| --- | --- | --- | --- | --- | --- | --- |
| Real model and provider route | P/E | P/E | P/E | P/E | P/E | P/E |
| Agent instructions, limits, context, inference, fallback |  | P/E | P/E | P/E | P/E | P/E |
| Tools, permissions, and protected effects | P/E | P/E |  | P/E | P/E | P/E |
| Files, repositories, Memory, and provenance | P/E | P/E |  | P/E | P/E | P/E |
| Skills, MCP, and multi-Agent configuration |  | P/E |  | P |  |  |
| Draft, Preview, diff, and publication snapshot |  | P/E |  | P |  | P/E |
| Managed Agents client compatibility |  |  | P/E |  |  |  |
| Session execution, trace, and committed history | P/E | P/E | P/E | P/E | P/E | P/E |
| Restart and recovery |  | P/E |  |  | P/E |  |
| Deployment, Run once, and schedule |  |  |  |  |  | P/E |
| Human authority and actionable handoff | P/E | P/E | P/E | P/E | P/E | P/E |

No row is complete merely because a control is rendered. The release gate for every
`E` cell is `UI input -> saved readback -> published snapshot -> runtime effect` where
the capability has a runtime effect. Failure at any edge blocks the video.

## Proofs that do not become videos

The following remain executable release gates:

- 01 Provider connection
- 03 Tool denial
- 04 Cross-Session Memory recall
- 05 AI-assisted authoring
- 06 State Machine enforcement
- 07 Clean all-in-one startup
- 10 Skill activation
- 11 Resource provenance
- 13 Managed API ingress
- 14 Session interrupt and archive
- 15 A2A discovery
- 16 Access issue and revoke
- 17 AI SDK and AG-UI history
- 18 MCP server export
- 19 Codex ACP execution
- 20 Tool identity and deferred loading

A proof returns to the public series only when it gains all five elements:

1. A recognizable job without invented biography or calendar pressure.
2. A consequence the viewer cares about.
3. A deliverable useful outside the product demo.
4. A visible human control point.
5. A recipient who can act on the result.

“The request was denied,” “the server is ready,” and “the protocol connected” are useful
evidence. They are not complete marketing stories.

## Script standard

### Opening

- Show the result or the decision tension within eight seconds.
- Name the task and failure consequence.
- Do not invent a person, company history, date, deadline, or meeting to add urgency.
- Do not open with a dashboard, feature category, or architecture term.

### Middle

- Keep setup below 40 percent of runtime.
- Show only operations that change the decision or establish trust in the result.
- Explain why an action matters. Never narrate where the cursor clicks.
- Use the real model for every claimed Agent result.
- Pair each important claim with visible UI and independent API readback.

### Ending

- End on the deliverable, not a saved form or success toast.
- Name the next action, owner, or recipient.
- Use one Aha line that still makes sense as a six to twelve second clip.
- Close with the brand only after the business result is visible.

## Copy standard

Every sentence has one job. Prefer short concrete verbs: decide, block, recall, assign,
review, publish, schedule, inspect.

Write:

- “NO-GO. The brief names both blockers.”
- “The conversation ends. The operating rule does not.”
- “The meeting starts with a decision, not a search for status.”

Avoid:

- “Unlock the power of next-generation agentic transformation.”
- “Seamlessly orchestrate robust, scalable workflows.”
- “Awaken showcases comprehensive end-to-end capabilities.”

Do not use inflated claims, vague superlatives, forced three-part slogans, or em dashes.
Do not claim a customer, deployment, business metric, certification, or compatibility
that the recording cannot prove.

## Visual and runtime standard

- Record the release build served by one `awaken all-in-one` origin.
- Use 1920 by 1200, H.264 High, yuv420p, 30 fps.
- Use only the time needed to close the story. Remove dead waits and repeated setup,
  but never cut the evidence, human boundary, result, or handoff to meet a time quota.
- Keep subtitles to one thought, no more than 12 English words or 100 characters.
- Focus the exact evidence named by the subtitle.
- Never leave a spinner, blank canvas, setup wait, or retry loop in the story.
- A stale source or harness hash blocks publication.
- A failed assertion produces a diagnostic artifact, never a polished MP4.

The brand close is:

> The conversation can end. The work stays ready for whoever comes next.

This line is a summary of what the viewer has already seen. It is not a substitute for
showing the result.
