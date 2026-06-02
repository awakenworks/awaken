# ADR-0033: Locked-field UI/API boundary for `AgentSpec`

- **Status**: Accepted
- **Date**: 2026-05-13
- **Depends on**: ADR-0010 (registry/resolve), ADR-0024 (admin console data
  router), ADR-0029 (tool description override)

## Context

`AgentSpec` carries three fields the admin-console agent editor treats as
*locked*:

- `backend` — canonical execution-backend selector. It supersedes
  `endpoint` for new config, but has the same retargeting blast radius:
  changing it can switch an agent from in-process Awaken execution to a
  remote backend or to another remote target.
- `endpoint` — remote-backend provenance. When present, the runtime
  resolver routes the agent's runs to a remote backend; when absent, the
  agent is local. Editing `endpoint` from the editor would silently
  re-target a builtin agent's runs to an arbitrary remote, which is a
  blast-radius the graphical editor is not the right surface for.
- `registry` — runtime-locality marker. Identifies which agent-spec
  registry the record belongs to. Editing it through the editor would
  detach the record from its source and break upstream sync.

The editor's `LOCKED_AGENT_FIELDS = ["backend", "endpoint", "registry"]` enforces
this client-side: the form path exposes no widgets for these fields, the
Raw JSON Apply rejects edits to them, `cloneAgentSpecForEditor` strips
them when duplicating a record, and the customized save path's
`PATCHABLE_AGENT_FIELDS` allowlist omits them.

At the API layer the picture is asymmetric:

- `registry` is **not** part of `AgentSpecPatch`. The struct's
  `#[serde(deny_unknown_fields)]` causes the server to reject any PATCH
  body containing `registry`. This matches the client lock.
- `backend` and `endpoint` **are** part of `AgentSpecPatch`. The server accepts
  programmatic backend replacement, endpoint upserts (`{"endpoint": {...}}`),
  and endpoint clears (`{"endpoint": null}`) directives. A passing integration test —
  `patch_overrides_null_clears_nullable_base_field` in
  `crates/awaken-server/tests/config_api.rs` — pins this behavior, and
  `merge_agent_spec` documents endpoint as a tri-state nullable patch
  field on equal footing with `context_policy`, `allowed_tools`, etc.

A reviewer of PR #189 flagged this asymmetry as a defense-in-depth gap:
"if the editor calls endpoint locked, the API should too, otherwise the
lock isn't a real contract." This ADR settles the question.

## Decision

**The lock on backend routing is a UX boundary, not an immutability boundary
or a security boundary. The asymmetry is intentional.**

Specifically:

1. **`backend` and `endpoint` remain patchable AgentSpec fields at the API layer.**
   `PATCH /v1/config/agents/:id/overrides` continues to accept upserts
   for `backend` plus upserts and clears for `endpoint`. The
   `AgentSpecPatch::backend` and `AgentSpecPatch::endpoint` fields stay.
   Removing either would be a breaking change requiring its own ADR.

2. **The admin-console editor continues to treat backend routing as locked.**
   No form widget edits `backend` or `endpoint`. Raw JSON Apply rejects edits
   to either. `PATCHABLE_AGENT_FIELDS` excludes both. This is the UX
   simplification:
   most operators have no business rebinding a builtin agent's remote
   backend through a graphical editor, and exposing that surface there
   would be one click away from operational mistakes.

3. **`registry` remains both UI-locked and API-locked.** Its locality
   semantics differ from endpoint — overriding it would detach the
   record from its source registry in ways the merge logic does not
   know how to undo. The API-level rejection (via `deny_unknown_fields`)
   is the right contract here.

4. **Programmatic clients retain backend and endpoint patch capability.** CLI tools,
   operations scripts, A2A federation glue, and other admin tooling can
   still upsert `backend` or upsert/clear `endpoint` overrides through the public PATCH
   endpoint. This supports legitimate use cases like:
   - Temporarily redirecting a builtin agent to a staging backend for
     pre-production validation.
   - Bulk re-pointing a fleet of customized agents after a backend
     hostname change.
   - Federation tooling that materializes remote-agent records with
     locally-supplied endpoint metadata.

5. **The admin console surfaces existing endpoint overrides.** When a
   loaded agent has `user_overrides.endpoint`, the editor renders a
   `data-testid="endpoint-override-banner"` warning explaining that the
   override was installed through the API and pointing operators at the
   `_clear` directive to remove it. This makes the boundary visible
   instead of letting the locked UI lie about effective state.

## Consequences

### Positive

- No breaking change to `AgentSpecPatch`. The `merge_agent_spec` tri-state
  semantic remains uniform across all nullable fields. Existing test
  `patch_overrides_null_clears_nullable_base_field` continues to pin the
  documented behavior.
- Programmatic clients keep a documented and supported surface for
  managing endpoint overrides without going through the editor.
- The graphical editor stays focused on the operations most users
  perform. Endpoint reconfiguration — a low-frequency, high-blast-radius
  operation — is intentionally pushed to a more deliberate channel.
- Operators are not misled: when an endpoint override exists, the
  banner explicitly says so and points to the correct removal path.

### Negative

- The asymmetry between UI lock and API lock requires this ADR plus
  in-source comments on every relevant call site (`AgentSpecPatch::endpoint`,
  `ConfigService::patch_agent_overrides`, `agent-editor-helpers.ts`
  `LOCKED_AGENT_FIELDS`). Without those, the design intent is not
  recoverable from code reading.
- New reviewers who don't read this ADR will reasonably re-raise the
  same defense-in-depth question. The banner + in-source comments aim
  to shorten that conversation.

### Future revisits

This decision is *not* sealed. Conditions that would warrant a new ADR
to reverse it include:

- Endpoint becomes a supply-chain or signing-anchor field whose
  immutability is load-bearing. At that point, removing
  `AgentSpecPatch::endpoint` would be the right move — but the
  migration path needs to be planned (e.g. dedicated
  `PATCH /v1/config/agents/:id/endpoint-binding` with stricter auth).
- An operational incident traces back to a programmatic endpoint
  override silently rebinding a production agent. Defense-in-depth
  arguments grow weight when there is a real incident behind them.

## Alternatives considered

### Alternative A: server-side hard-lock on `endpoint`

Reject any PATCH body containing `endpoint`, regardless of value.
Equivalent to position B in PR #189 review thread.

Rejected because:

- Breaking change. The `patch_overrides_null_clears_nullable_base_field`
  test in `origin/main` pins the inverse contract, and unknown
  programmatic clients may rely on it.
- Removes a legitimate operational capability (staging redirects,
  bulk re-binding) without offering a replacement surface.
- The UI lock is sufficient for the actual UX concern (preventing
  accidental endpoint edits through the editor). The defense-in-depth
  argument would only be load-bearing if there were a security
  boundary at stake, which there is not — anyone with an admin token
  can already do anything through the API.

### Alternative B: server permissive, no documentation

Keep the current behavior but do not document the asymmetry. Equivalent
to position A in PR #189 review thread.

Rejected because:

- Reviewers reasonably interpret the UI lock as the contract and
  re-raise the asymmetry as a bug every PR. The cost of clarification
  conversations exceeds the cost of writing this ADR once.
- Operators don't see existing endpoint overrides because the editor
  hides them, leading to surprising behavior at runtime.

### Alternative C: tri-state — allow upsert, reject clear

Accept `PATCH {"endpoint": {...}}` but reject `PATCH {"endpoint": null}`,
forcing clears to go through `_clear: ["endpoint"]`.

Rejected because:

- Still breaks `patch_overrides_null_clears_nullable_base_field` and
  the documented `merge_agent_spec` tri-state contract.
- Splits the semantic surface (null vs `_clear`) without a clear win.
- Operationally indistinguishable from Alternative A.

## Implementation pointers

The contract is enforced and surfaced at five sites:

| Site | Role |
|------|------|
| `crates/awaken-runtime-contract/src/agent_spec_patch.rs` — `AgentSpecPatch::endpoint` | Long-form rationale in field doc comment |
| `crates/awaken-server/src/services/config_service.rs` — `patch_agent_overrides` | Contract-surface note in method doc comment |
| `crates/awaken-server/tests/config_api.rs` — `patch_overrides_null_clears_nullable_base_field` | Contract-pin test; preamble flags it as load-bearing |
| `apps/admin-console/src/lib/agent-editor-helpers.ts` — `LOCKED_AGENT_FIELDS` / `lockedFieldChange` | Client-side lock + normalization contract |
| `apps/admin-console/src/pages/agent-editor-page.tsx` — `endpoint-override-banner` | Visibility for existing overrides |

Changing any of these without updating the others — or without updating
this ADR — should fail review.
