# Wiki Index

Use this wiki to find compact facts, then read the linked owner before changing
code or long-form design.

## Orientation

- [Design corpus guide](../README.md) - rules for this design corpus
- [Design status](../STATUS.md) - document readiness and catalog policy
- [Requirements coverage](../requirements-coverage.md) - whole-runtime coverage map
- [Architecture overview](../design/architecture-overview.md) - bounded contexts and vocabulary
- [Config to run execution flow](../design/config-to-run-execution-flow.md) - explicit model-provider/model/model-pool/agent graph, publication, catalog install, executable snapshot selection, activation/context split, resolution, execution, and commit flow
- [Key design decisions](../design/key-design-decisions.md) - load-bearing architecture decisions
- [Runtime behavior](../design/runtime-behavior.md) - run lifecycle, live state apply, durable commit, effects, and events
- [Runtime scenario validation](../design/runtime-scenario-validation.md) - GWT scenario ids, test mapping, and scenario organization
- [Invariants](../INVARIANTS.md) - enforceable guardrails

## Ownership

- [Document ownership](document-ownership.md) - source-to-wiki ownership map

## Facts By Theme

- [Neutral waist](neutral-waist-facts.md) - runtime execution ports and the data-only config edge
- [Config to run execution flow](config-to-run-execution-flow-facts.md) - explicit config graph, configuration publication, external publication roles, catalog install, executable snapshot selection, activation/context split, resolution, execution, and commit handoffs
- [Runtime interface boundaries](runtime-interface-boundaries-facts.md) - runtime role split, activation/context/snapshot split, catalog install boundary, executable snapshot contract, plugin seams, tool decision ladder, and simple-design checks
- [Runtime behavior](runtime-behavior-facts.md) - run phases, live state apply, durable commit, state/effects, plugins, scheduled work, eval
- [Tool and capability](tool-and-capability-facts.md) - descriptors, neutral ToolExecutor port, capability checks, permission boundaries
- [Runtime explicit boundaries](runtime-explicit-boundaries-facts.md) - protocol adapters, permissions, binding, errors, and packaging checks
- [Run ingress and message delivery](run-ingress-message-delivery-facts.md) - direct ingress, durable ingress, pending input, and message recovery
- [Product protocol and sessions](anthropic-alignment-and-sessions-facts.md) - anti-corruption adapters and public projections
- [Credentials and vaults](credentials-and-vaults-facts.md) - opaque refs, selection, availability
- [Resources, memory, files, and skills](resources-memory-files-skills-facts.md) - logical resource refs

## Lessons

- [Engineering lessons](engineering-lessons.md) - durable engineering rules

## Maintenance

- [Wiki guide](README.md) - OKF-compatible wiki rules
- [Agent instructions](AGENTS.md) - contributor instructions for agents
- [Wiki maintenance notes](maintenance-notes.md) - durable maintenance policy
- [Wiki update log](log.md) - OKF update history
