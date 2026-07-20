# ADR-0061: Selectable identity and platform-managed resource scopes

- Status: Accepted
- Date: 2026-07-20
- Depends on: ADR-0038, ADR-0042, ADR-0048, ADR-0051, ADR-0053

## Context

The local product previously mixed three unrelated concerns:

- whether a user signs in;
- which IAM deployment evaluates authorization; and
- which Workspace owns Files, Memory, Skills, Agents, Sessions, Artifacts and
  worker activity.

Several adapters also used a compiled `"default"` Workspace, process-local owner
indexes, or process-local version projections. An id could therefore outlive its
owner index, and the rich Memory/Skill API could lose history on restart.

## Decision

### Identity modes

The local product exposes two ordinary choices:

1. **No login** (default): single-user local operation. The installation owns a
   generated Workspace coordinate, persisted beside durable storage.
2. **Sign in with Awaken**: reuse the Awaken Cloud account credential cached by
   `awaken-iam-client`. JWT verification and authorization use awaken-iam's
   remote trust root/PDP. An explicit request bearer overrides the cached login.

A third integration mode, **self-managed IAM**, remains supported through
explicit deployment configuration. It is an operator feature documented only on
awakenworks.com; local product copy and onboarding do not promote it.

`AWAKEN_IDENTITY_MODE` accepts `no-login`, `awaken-cloud`, and `self-managed`.
Legacy `AWAKEN_MGMT_IAM=embedded` remains an alias during migration. Cloud mode
fails closed when no cached/explicit login credential or JWKS trust root is
available. Self-managed mode requires a durable management directory.

An API key is a credential, not a permission model. Authentication resolves a
principal and credential constraints; the PDP still evaluates principal,
action, target scope, resource facts, and active policy version.

### Scope and policy ownership

Resource scope is resolved at the edge and carried as trusted request/session
context. Resource services never infer a tenant from an id and never contain a
fixed Workspace constant.

```text
credential/path + resource address
               |
               v
        PEP target resolver
        (trusted scope context)
               |
               +----> PIP: durable owner/parent facts
               |
               v
      awaken-iam PDP <---- PAP active policy/profile
               |
        allow / deny / obligation
               |
               v
       scoped repository operation
```

Action-to-scope applicability is centralized in awaken-iam's versioned
authorization profile/resource model. A route is classified as Workspace,
Project, or leaf Resource once; the PEP resolves the concrete coordinate and the
PDP verifies it against the active profile. Project routes submit
`ScopeRef::Project`; they are not silently reduced to Workspace scope.

The PAP activates immutable profile revisions atomically and supports rollback.
Embedded and remote deployments consume the same profile/snapshot format. An
environment variable may select a profile or IAM endpoint, but cannot redefine
individual action/scope rules.

This paragraph is the cross-repository completion contract, not a claim that the
currently pinned IAM wire API already implements it. Today
`ResourceModelRegistration` is additive and the management PEP keeps its route
target classification in one compiled mapping. ADR-0010 in awaken-iam therefore
marks whole-profile validate/activate/fetch/rollback as **Proposed**. Until that
API is implemented, published, and consumed here, roles and grants remain
adjustable but action-to-scope applicability is not atomically replaceable or
rollbackable. A release must not describe that narrower state as completed
authorization governance.

### Resource adapters

- **Sessions/Runs/Workers:** creation persists the verified Workspace; runtime
  preparation records it per thread. Claims and completion remain authenticated,
  lease-fenced operations over a scoped work row.
- **Memory:** the blob namespace and durable `MemoryStoreDef.workspace_id` carry
  ownership. Unknown/legacy owner rows return 404. Version history is append-only
  SQLite in durable mode and redaction survives restart.
- **Files:** content hashes address tenant-neutral bytes. A separate durable
  many-to-many `(file, id, workspace)` projection grants visibility. Knowing a
  hash is never authorization. Deleting one tenant's reference does not delete a
  blob still owned by another tenant.
- **Artifacts:** inherit the producing session's recorded Workspace and receive a
  File ownership grant when harvested. Listing requires the session Workspace to
  equal the request Workspace.
- **Skills:** the SkillStore and cache are Workspace-keyed. The rich Skill object
  and complete version projection are persisted in durable mode; retrieval and
  capability advertisement use the session Workspace.
- **Agents:** config aggregates already use scoped repositories. Auxiliary
  agent-resource bindings now use a composite `(workspace, agent)` key in memory,
  SQLite, and Postgres; compile and runtime mount resolution pass the same scope.
- **MCP/Profile/Webhook config:** ownership is intrinsic to the durable aggregate
  row. `AgentMcpConfig` also carries the edge-stamped Workspace, and authoring,
  reading, dry-run resolution, and session preparation verify the same owner.
  The former `ResourceOwners`/`resource-owners.sqlite` side projection is removed;
  it duplicated aggregate truth, could drift, and was not needed for authorization.

Flat no-login requests receive only the installation `WorkspaceScope` ownership
context. They do not receive a caller `RequestTenancy` selector. Authenticated
middleware may therefore replace the ownership context with credential authority;
explicit `/v1/workspaces/{workspace}/...` addressing remains a selector and is
fenced against that authority.

## Consequences

- Durable mode restores ownership and rich version APIs after restart.
- Equal resource ids can safely exist in multiple Workspaces without treating an
  id as a secret.
- Resource services know only trusted scope coordinates and repository filters;
  IAM credentials, roles, policy syntax, and deployment topology stay outside the
  resource domain.
- Missing authentication is allowed only in the explicit no-login composition.
  Missing scope, ownership, resource-model registration, or policy is deny-by-default
  in authenticated compositions.
- Org/Workspace/Project policy changes are made once in awaken-iam PAP and
  activated as a version, instead of redeploying every resource service.

## Remaining migration gates

- Standalone legacy/raw router compositions still contain compatibility
  `DEFAULT_SCOPE` fallbacks. The supported product composition always injects
  the generated/persisted platform Workspace, but those fallbacks must become a
  required scope input or fail closed before the individual routers are claimed
  safe for arbitrary multi-tenant embedding.
- Skill object/version persistence is durable, but complete bundle
  materialization and runtime enforcement of `allowed_tools` remain the separate
  runtime capability-policy slice recorded by ADR-0036. IAM ownership checks do
  not substitute for that execution permission gate.
- The awaken-iam whole-profile lifecycle described above must land before
  action/scope rules can be replaced and rolled back as one centrally managed
  unit.

## Amendment — 2026-07-20: Runtime stops at Workspace

The Runtime authorization hierarchy is `Org -> Workspace`; it does not expose
or evaluate a Project scope. This supersedes the earlier paragraph that required
Project routes to submit `ScopeRef::Project` and the Project wording in the
consequences above. A `project_id` occurring in compatibility/config APIs is
resource data inside its owning Workspace and those routes authorize with
`workspace.read` / `workspace.write` at `ScopeRef::Workspace`.

Single-machine composition hides Org and uses `org_default`, while its Workspace
is generated and persisted as `platform-workspace-id` (or explicitly supplied by
the platform). Embedded IAM registers that one `Org -> Workspace` edge and
migrates the legacy Global bootstrap binding to the hidden Org. Awaken Flow is a
different bounded context and retains its own `Org -> Workspace -> Project`
authorization hierarchy.

The whole-profile lifecycle is now implemented in the pinned awaken-iam
revision: product-qualified action namespaces, immutable durable revisions,
validation, CAS activation, hydration, and rollback are shared by embedded and
remote deployments. The Runtime management profile owns only the qualified
`workspace.*` and `apikey.*` vocabulary and permits those actions only at
Workspace targets.

Skill `allowed_tools` is enforced after the platform tool gate as a session-local
monotonic intersection. It can only remove authority and can never grant or
restore a tool denied by IAM/platform policy. Full bundle materialization remains
content delivery and sandbox packaging work; it is not an authorization bypass.
