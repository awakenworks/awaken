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

The typed deployment field `identity_mode` accepts `no-login`, `awaken-cloud`,
and `self-managed`. Retired process-environment aliases do not participate in
deployment resolution. Cloud mode fails closed when no cached/explicit login
credential or JWKS trust root is available. Self-managed mode requires a durable
management directory.

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
Embedded and remote deployments consume the same profile/snapshot format. Typed
deployment configuration may select a profile or IAM endpoint, but cannot
redefine individual action/scope rules.

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
- The pre-1.0 API-local `resource-api.db` registry and its startup importer are
  retired. The 1.0 schema baseline has one owner per Resource aggregate; product
  processes neither dual-read nor infer canonical state from that obsolete sidecar.
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
restore a tool denied by IAM/platform policy. Complete binary-safe bundles are
now hash-verified and materialized under the Session's `.skills` tree from the
exact version frozen in `ResolvedSessionResources`; that content operation is not
an authorization bypass.

## Amendment (2026-07-23): MCP ownership follows the Agent aggregate

The `AgentMcpConfig` sentence in the Resource adapters section records a retired
intermediate design. MCP endpoint authoring now lives only in the
Workspace-scoped `AgentConfig` aggregate as typed `AgentMcpServerBinding`
values. An optional credential is an exact secret-free
`CredentialRef { id, revision }`; it does not transfer credential ownership to
the Agent context.

At publication, the configuration application invokes a
`CredentialReferenceValidator` implemented by the control-plane credential
adapter. It verifies the trusted Workspace, Active status and exact revision
without exposing material. Runtime preparation rechecks Workspace and revision
before materialization. IAM authorization remains at the PEP; neither check is
an authorization grant.

The standalone `McpServerDef` / `AgentMcpConfig` tables, repositories, routes and
DTOs are removed by the scoped admin migration. Historical migration files stay
append-only. A Session persists the effective published-or-explicit-override MCP
projection needed for restart; that immutable Session value is not another
authoring aggregate.

## Amendment (2026-07-26): Hosted Management uses workload identity

`awaken-cloud` identity has two compositions over the same
`RemoteManagementAuthz` adapter. A local interactive process may use the cached
Cloud login as its default request bearer and as the remote PDP carrier. A
server-mode Awaken Management process has no local user and instead requires a
dedicated projected service-token file; browser requests must present their own
bearer. The projected token is read by the IAM HTTP transport for every PDP
attempt, so atomic replacement rotates it without restarting Management.

```text
browser bearer -> local JWT verification -> subject principal
Management projected token file -> remote PDP carrier
subject + exact Workspace + action -> IAM decision
```

| Mode | Request bearer | Management token file | Result |
|---|---|---|---|
| local | absent | absent | cached Cloud login may authenticate |
| local | valid | absent | explicit bearer overrides cached login |
| server | valid | readable, non-empty | authenticate user, authorize through IAM |
| server | absent/invalid | any | `401`; workload identity is not a user identity |
| server | valid | missing/empty/expired | fail closed at PDP; no local authorization fallback |
| server | any | Flow or another workload's token | IAM audience/subject policy denies |

Server mode rejects an inline service token, a missing token file, and dual
inline/file configuration before serving. Management never mounts a Flow token
or an IAM signing key. The `awaken control` process mounts only the existing
authoring/control router; it does not mount Session, protocol, Run ingress,
Worker transport, or local dispatch routes and therefore cannot become a second
Runtime authority beside a hosted Coordinator. Runtime/resource stores remain
unaware of the deployment credential.

Managed PostgreSQL schema changes have one operational writer. The explicit
`awaken database migrate` command applies the existing scoped bundles; a
server-mode `awaken control` process opens the same bundle-specific stores
in verification mode and performs no DDL. Local SQLite `awaken all-in-one` continues
to migrate on open so a new local install stays zero-configuration.

```text
deployment migration Job -> scoped migration run_bundle -> ledger + DDL
application Pod          -> scoped migration verify_bundle -> serve or fail
```

| Mode/caller | Schema | Decision |
|---|---|---|
| local `start` | absent/pending | apply embedded SQLite migrations and start |
| migration command | absent/pending | apply PostgreSQL bundles idempotently |
| migration command | current | no-op success |
| server `management` | absent/pending | fail closed without DDL |
| server `management` | current | start control-only surface |
| either | checksum drift/unknown version | fail closed |

The table is executable test design, derived from this causal graph:

```text
caller mode -> migrate or verify -> scoped ledger state -> apply / serve / fail
                                     \-> verify never creates ledger or tables
```

Every Postgres `connect_existing` used by server mode must therefore exercise
the absent-ledger failure, prove the ledger is still absent afterward, then pass
after the same bounded context's canonical bundle is migrated. SQLite and
Postgres Sandbox Policy adapters use that same portable bundle rather than
maintaining parallel raw DDL.

## Amendment (2026-07-28): Management authorization is a release contract

The Management bounded context is the sole owner of the
`awaken.runtime.management` and `awaken.runtime.resources` action, scope and
role-grant contracts. Their existing embedded-IAM constructions are exposed as
deterministic, side-effect-free `management_authorization_profile()` and
`management_resource_authorization_profile()` functions. Embedded IAM consumes
those functions directly; the `awaken control iam profile` commands only
serialize the same values for a deployment-owned PAP.

```text
Management action/scope/role definitions
  -> management_authorization_profile()
       |-> embedded IAM activation
       \-> awaken control iam profile
            -> immutable release JSON
            -> hosted PAP validation/CAS activation

Management resource action/scope/role definitions
  -> management_resource_authorization_profile()
       |-> embedded IAM activation
       \-> awaken control iam profile resources
            -> immutable release JSON
            -> hosted PAP validation/CAS activation
```

Cloud or another host must consume the contract emitted by the exact Management
image it deploys. It must not compile a second action matrix from a separately
pinned library revision. The command does not load deployment configuration,
read credentials, open storage, contact IAM, or publish policy.

| Invocation | Configuration/storage/network | Outcome |
|---|---|---|
| `management iam profile` | unavailable | deterministic `awaken.runtime.management` JSON |
| `management iam profile resources` | unavailable | deterministic `awaken.runtime.resources` JSON |
| same exact image, repeated | unavailable | byte-identical JSON |
| `management iam profile` with extra input | unavailable | usage failure, no JSON |
| embedded IAM startup | local durable state | activates the same generated document |
| hosted Management startup | remote IAM configured | does not publish or mutate a profile |

Cross-product automation binds the product-owned
`awaken.runtime.management:agent_publisher` role. That role grants only
`workspace.*`; it deliberately excludes `apikey.*`. Human Workspace owners may
hold `workspace_admin`, but a Flow workload must not inherit credential
administration merely because both capabilities use the Management API.

## Amendment (2026-07-29): Hosted Runtime lifecycle profile is Awaken-owned

The `run.create`, `run.read`, `run.resume`, and `run.cancel` vocabulary belongs
to Awaken even when a closed platform hosts the Coordinator. Awaken therefore
exports one deterministic `hosted_runtime_authorization_profile()` and projects
it through `awaken control iam profile runtime`.

```text
Awaken Hosted lifecycle vocabulary
  -> hosted_runtime_authorization_profile()
  -> exact Awaken image release JSON
  -> hosting PAP validation/CAS activation
  -> hosting-owned exact Workspace role bindings
```

The profile defines only two roles: `awaken.runtime:workspace_admin` for a human
Workspace owner and `awaken.runtime:agent_executor` for a product workload.
Both receive only `run.*` lifecycle authority at Workspace scope. Provider
credentials, Management configuration, IAM decisions, billing and Gateway
leases are absent. A host must consume the profile emitted by the exact Awaken
image; it must not reproduce this action/role matrix in closed code.

Each concrete lifecycle action carries its own Workspace scope rule. This is
the canonical IAM profile shape: action registration remains explicit, while
the role grant may use the bounded `awaken.runtime::run.*` pattern. The release
contract is validated through the IAM PAP before it is considered deployable.

This release projection does not merge hosted closed code into Awaken. The open
Management image remains independently deployable; a hosted release composes
its immutable image and contract with external IAM lifecycle management.

## Amendment (2026-07-30): hosted suite navigation is inert deployment presentation

An Awaken console may be delivered alone or as one product in a hosted suite.
Hosted composition supplies an optional `suite_hub_url` through the existing
typed deployment document. The CLI validates that it is an exact HTTPS URL
(loopback HTTP is allowed only in local mode) without credentials, query, or
fragment, then projects Foundation's `SuiteNavigation` from
`/.well-known/awaken-suite-navigation`.

```text
deployment suite_hub_url
  -> Awaken composition root validation
  -> inert same-origin SuiteNavigation
  -> optional product menu in the Awaken-owned shell
  -> full-page navigation to the external hub
```

The projection carries no Org, Workspace, account, entitlement, sibling
product URL, or Billing fact. IAM remains the authentication/authorization
authority; the hub remains the product launch authority; Awaken remains the
Agent/Session/Run authority. Standalone mode returns `hub_url: null` and does
not guess from Host, issuer, referrer, or an OAuth token. The browser menu is a
navigation affordance, never an authorization decision or a second product
topology.

## Amendment (2026-07-31): hosted browser entry returns to the account hub

The same inert `SuiteNavigation` prevents an unauthenticated direct hosted URL
from falling into Awaken's standalone setup or default Workspace presentation.
Before local browser setup, the console reads the same-origin projection. An
exact hub plus no canonical product session bearer navigates to that opaque hub;
an existing bearer continues; `hub_url: null` retains the standalone path.
Projection failure is visible and retryable and never guesses a login mode,
OAuth coordinate, tenant, or Workspace.

| Suite projection | Product bearer | Result |
|---|---|---|
| exact hub | absent | navigate to the exact hub; Cloud/IAM owns sign-in |
| exact hub | present | enter the hosted console |
| null | absent/present | retain standalone behavior |
| unavailable | any | fail closed with retry |

Awaken still does not construct OAuth URLs or know Cloud product topology. The
Cloud hub remains responsible for returning through the existing product launch
capability with the exact Workspace route.

### Amendment (2026-08-10): direct entry carries opaque browser continuation

The direct-entry decision is generated from Foundation beside the shared
`SuiteNavigation` contract, eliminating the handwritten copy previously kept in
both Awaken and Flow. For an unauthenticated hosted browser it appends the
current absolute Awaken URL as an opaque `continue` value on the exact hub; a
present product bearer and standalone mode continue unchanged.

Awaken does not validate or persist that value, infer a Cloud domain, or
construct an OAuth route. Cloud accepts it only when the origin and leading
Workspace path equal the authenticated tenant's current Awaken coordinates,
then returns through IAM's existing browser PKCE adapter. Invalid or foreign
continuations stop at Products.
