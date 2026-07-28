# ADR-0002: Resolver Role Demarcation

- Status: Accepted
- Depends on: ADR-0001
- Supersedes: none

## Context

Snapshot lookup, executable-Run materialization, and child-Agent execution were
previously described with overlapping `*Resolver` language. That made docs and
call sites hide which domain operation was occurring.

## Decision

Only two runtime lookup/materialization interfaces retain the `Resolver` suffix:

| Role | Input → Output | Layer |
|---|---|---|
| `AgentSnapshotResolver` | `ExecutableAgentSnapshotId` → `ExecutableAgentSnapshot` | configuration/catalog boundary |
| `RunResolver` | pinned `ExecutableAgentSnapshot` → `ResolvedRun` | Runtime Core |

Delegation is a Run-domain service, not resolution. It is named
`RunDelegationService`. As amended by ADR-0057 Phase E, local and remote
targets are not different delegation implementations: the service creates or
resumes one ordinary child Run from the target publication, and the child's
immutable backend selects Native, ACP, or A2A through the common
`RunAttemptExecutor` registry. Call sites say `start`, `resume`, `Ended`, and
`continuation`, matching the Agent domain.

Documents and call sites name the concrete role rather than saying only “the
resolver”. Auxiliary provider/config resolvers are local implementation roles and
must state what value they resolve.

## Consequences

- Snapshot identity lookup cannot be confused with Runtime materialization.
- Delegated child Runs use execution language and carry typed parent/call/child/
  result identities.
- A local child executes as an ordinary `RunActivation`/`RunAttemptExecutor` over
  `RunDispatch`; `DelegationOrigin` records parentage and `session_thread_id`
  retains parent-Session environment affinity. There is no child executor, child
  store, or delegated completion helper (G40).
- Local and remote delegated Agents share the same child-Run lifecycle;
  protocol routing stays at the attempt execution edge outside Runtime Core.
- A new `*Resolver` helper must name the value it resolves.

## References

- [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md)
- [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md)
- ADR-0001 D3 (internal vocabulary consistency)
