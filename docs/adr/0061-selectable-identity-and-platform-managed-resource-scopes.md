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

The same product publisher identity is bound to namespace-owned companion
roles: `awaken.runtime.management:agent_publisher` permits Agent Workspace
mutation and model-supply discovery, while
`awaken.runtime.resources:agent_publisher` permits only `skill.*`. The external
product must materialize the immutable Skill bundle in the exact execution
Workspace before publishing an Agent that pins the returned Skill version.
Neither role grants `file.*`, API-key administration, or model-supply mutation.
This keeps one publisher principal and one exact Workspace while each profile
owns its action and role vocabulary, as required by profile validation.

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

### Amendment (2026-08-15): the route owns hosted bootstrap scope

The presence of a product bearer is not proof that it is current or authorized
for the browser route. In hosted mode, an exact `/w/{workspace}` route is parsed
before the first management request and is the only browser-selected Workspace
input. Persisted presentation preferences never address an authorization
request. The existing Workspace-context endpoint verifies that exact route
through the existing authentication, tenancy fence, action mapping and PDP.

An absent route returns through the opaque Cloud `/entry` coordinate even when
session storage contains a bearer, allowing Cloud to select the current Org and
issue an exact product launch. A `401` removes only the product-origin bearer
and returns through the same entry. A `403` is terminal for that exact route and
shows the existing correlation id; it does not start an authentication loop.
Standalone mode retains the canonical local browser-session bootstrap.

| Hosted route | Bearer | Context probe | Result |
|---|---|---|---|
| absent | any | not sent | Cloud entry selects exact product Workspace |
| exact | absent | not sent | Cloud entry preserves exact continuation |
| exact | present | exact `2xx` | mount product router |
| exact | present | `401` | clear product bearer; Cloud entry |
| exact | present | `403` | bounded access-denied presentation with request id |
| exact | present | other failure | fail closed with explicit retry |

### Amendment (2026-08-12): hosted publication does not impersonate a user

A hosted Control process authenticates PDP calls with its projected workload
credential and authenticates browser requests with each request bearer. It has
no cached interactive Cloud credential and therefore must not construct the
local brokered-inference fallback client merely because Cloud models are
published. A hosting composition that injects the authoritative model resolver
and catalog discovery uses those ports directly; the interactive broker client
remains confined to the local published-provider composition that materializes
models on behalf of its signed-in user.

Likewise, a split Coordinator never constructs that interactive client: it owns
Session and Run orchestration while the hosted Worker/Gateway boundary owns
model realization. AllInOne retains the existing brokered client because it
co-locates Control and runtime materialization. Missing hosted catalog input is
represented as unavailable discovery, never repaired with a workload token
masquerading as a user token.

### Amendment (2026-08-13): automation may discover executable model supply

The product-owned `awaken.runtime.management:agent_publisher` role is the
least-privilege cross-product automation contract. Flow needs to read the
executable model and Provider descriptor projections before it can author an
Agent that Management can publish. Those reads are already classified by the
one Management route policy as
`awaken.runtime.management::model_supply.read`; they are not Workspace reads.

The authoritative `management_authorization_profile()` therefore grants the
publisher exactly `workspace.*` and `model_supply.read` at Workspace scope. It
still excludes `apikey.*`, `model_supply.connect`, `model_supply.write`, and
`model_supply.*`. A hosting platform binds this exact role; it must not repair a
missing discovery permission by binding the broader
`hosted_workspace_admin` role or by defining a parallel policy.

```text
Flow workload -> executable-models / provider-descriptors GET
              -> Management route policy: model_supply.read
              -> active Awaken release profile + exact Workspace binding
              -> allow read-only discovery / deny supply mutation and credentials
```

The profile document remains the single static policy owner. At runtime the
PEP supplies the authenticated Flow principal and exact Workspace, the remote
PDP evaluates the active revision, and denial remains terminal unless the
deployment reconciles that same release profile and binding. Profile
reconciliation appends and activates a new immutable revision; it does not
edit an active revision in place.

### Amendment (2026-08-14): hosted credential ingress is a separate authority

Flow owns tenant-authored business-resource configuration, but the canonical
secret material for those resources lives in Awaken's existing
Workspace-scoped Credential Vault. The Flow workload must therefore be able to
create, reread, rotate and archive generic business credentials through the
Management credential routes without inheriting Agent publication, model
supply, File, Skill or Run authority.

The authoritative `management_authorization_profile()` defines the independent
`awaken.runtime.management:credential_ingress` role. Its sole grant is
`awaken.runtime.management::apikey.*` at Workspace scope. The existing
`agent_publisher` remains credential-free, and a hosting platform binds both
roles only when the same product workload owns both Agent publication and
credential ingress at one exact execution Workspace. The broader human
`hosted_workspace_admin` role is not an automation substitute.

```text
Flow business credential command
  -> existing Management credential route and Credential Vault
  -> apikey.read / apikey.write at the request Workspace
  -> active Awaken release profile + exact credential_ingress binding
  -> secret-free response / durable Vault mutation
```

| Binding and target | Action | Result |
|---|---|---|
| credential ingress at exact Workspace | `apikey.read` / `apikey.write` | allow |
| credential ingress at another Workspace | any | deny |
| credential ingress at exact Workspace | Workspace, model, File, Skill or Run action | deny |
| agent publisher without credential ingress | `apikey.*` | deny |

This is a new role in the existing Management profile, not a second credential
API, Vault, policy namespace or hosting-owned action matrix. Profile
reconciliation and the existing credential PEP remain the only policy and
enforcement paths.

## Amendment (2026-08-15): one Workspace authorization language

Control and Resources remain separate bounded contexts and retain their own
applications, stores, migrations, ownership facts, and lifecycle rules. That
internal DDD boundary does not justify two user-visible policy namespaces for
operations that all target the same Awaken Workspace, use the same relying
party, and are assigned together to the same humans and product workloads. The
separate `awaken.runtime.management` and `awaken.runtime.resources` profiles
duplicated `workspace.*`, split one publisher intent across companion roles,
and allowed a hosted owner to receive only half of the required authority.

Awaken now owns one `awaken.workspace` authorization profile for Control and
Resources HTTP operations. The existing `awaken.runtime` profile remains
separate because Run lifecycle has a different relying-party boundary,
audience, caller lifecycle, and operational purpose.

Static structure:

```text
Control routes ---- local actions --\
                                    +-> Workspace PEP -> awaken.workspace profile
Resources routes -- local actions --/                        |
                                                             +-- hosted_admin
                                                             +-- publisher
                                                             `-- credential_ingress

Hosted Run routes -> Runtime PEP -> awaken.runtime profile
```

The three hosted Workspace roles are intentionally small:

| Role | Grants at an exact Workspace | Intended holder |
|---|---|---|
| `awaken.workspace:hosted_admin` | `workspace.*`, `apikey.*`, `model_supply.read`, `file.*`, `skill.*` | tenant Workspace owner |
| `awaken.workspace:publisher` | `workspace.*`, `model_supply.read`, `skill.*` | Flow publication workload |
| `awaken.workspace:credential_ingress` | `apikey.*` | Flow credential-ingress workload |

Self-managed named roles are still projected from the one existing role
catalog, but all their action and role identifiers are qualified exactly once
under `awaken.workspace`. A role id names a stable capability set; it never
contains an Org id, Workspace id, or scope kind. The active profile declares
which scope kinds an action accepts, while the IAM role binding carries the
concrete Workspace instance. This keeps role vocabulary finite and makes
cross-Workspace isolation explicit in data rather than encoded into strings.

`workspace_authorization_profile()` is the sole producer. Embedded IAM consumes
it directly and `awaken control iam profile` serializes it. The separate
`management_resource_authorization_profile()` producer and `profile resources`
CLI projection are removed; no alias or request-time dual-profile fallback is
kept. Both `RouteAuthz::Scoped` and `RouteAuthz::Resource` retain their route
ownership meaning but qualify their local actions through the same Workspace
namespace at the PEP boundary.

Dynamic cutover:

```text
exact Awaken image exports awaken.workspace
  -> PAP validates and activates the new profile
  -> hosting reconciles new exact-Workspace bindings
  -> readiness proves profile + required bindings + scope graph
  -> Awaken PEP rollout emits only awaken.workspace actions
  -> hosting revokes superseded role bindings
  -> PAP CAS-retires both legacy active heads
```

| New profile | New binding | New PEP | Legacy heads/bindings | Result |
|---|---|---|---|---|
| absent/invalid | any | any | any | fail before rollout |
| active | missing/foreign | any | retained | non-ready; repair exact binding |
| active | exact | old | retained | old service remains authorized during bounded cutover |
| active | exact | new | retained | new chain proven; finalization eligible |
| active | exact | new | retired/revoked | terminal single-path state |
| any | any | new | retired before new chain proof | forbidden rollout ordering |

Legacy immutable revisions remain audit evidence, but their active heads and
bindings do not. This is a release migration, not a permanent compatibility
mode.

## Amendment (2026-08-15): two simple human levels, atomically composed

Hosted users need read-only Run visibility as well as Workspace/Resource
visibility. IAM intentionally confines every profile's role ids to its own
namespace, so Awaken keeps Workspace and Runtime capabilities separate and the
hosting composition maps them to two user-facing access levels:

| Access level | Workspace role | Runtime role |
|---|---|---|
| Member | `awaken.workspace:workspace_user` | `awaken.runtime:workspace_user` (`run.read`) |
| Administrator | `awaken.workspace:hosted_admin` | `awaken.runtime:workspace_admin` (`run.*`) |

The host must replace each level as one atomic exact-Workspace role bundle; it
must not expose the internal pair as two independent UI toggles or execute a
revoke/grant sequence. `awaken.runtime:agent_executor` remains a separate
workload role because it expresses execution delegation rather than human
membership. Role ids remain finite intent names, while IAM bindings carry the
concrete Workspace instance.

This extends the 2026-07-29 runtime profile with the read-only human role. It
does not merge the Runtime and Workspace PEP/profile boundaries or change
self-hosted role behavior.

## Amendment (2026-08-16): legacy Workspace authorization has a terminal cut

The one-Workspace-language migration is complete only when request processing
cannot reinterpret a retired token or role namespace. Embedded startup now
performs one fail-closed release migration before live-PDP hydration:

1. It inventories every binding in the two retired profile namespaces without
   writing.
2. A complete authority-equivalent pair is replaced by one canonical
   `awaken.workspace` binding. If a prior attempt already committed that exact
   canonical binding, remaining legacy rows are cleanup remnants and may be
   removed idempotently.
3. An incomplete pair or unknown legacy role refuses startup with the exact
   principal and scope for operator repair. It is never hydrated through a
   request-time alias and is never widened to the canonical role.
4. After the binding inventory is empty, both retired profile heads are retired
   and live hydration accepts only canonical role identifiers.

Management API-token authentication likewise admits only the Awaken-owned
`sk-awaken-` format. The upstream IAM parser may retain broader decoding for
other relying parties, but Awaken Control does not expose `sk-ant-` as a second
management credential format. New minting was already canonical; this amendment
removes the final read-side compatibility authority.

This adds no profile, PDP, token repository, or migration database. It closes
the bounded migration in the existing embedded-IAM composition root and keeps
immutable legacy profile revisions as audit evidence only.

## Amendment (2026-08-19): Builder is a product access level, not a subscription

Awaken owns the permissions of a human inside one Awaken Workspace. A hosting
platform may sell a number of Builder seats, but that commercial capacity is
not a role and grants no permission by itself. Conversely, assigning a product
role does not create or modify a subscription. The hosting composition must
check its purchased capacity before it writes the existing IAM binding.

Awaken exposes exactly three human access levels. Each is an intent composed
from the existing Workspace and Runtime authorization profiles; the profiles
remain separate because they protect different relying-party boundaries.

| Access level | Workspace role | Runtime role | Product authority |
|---|---|---|---|
| Viewer | `awaken.workspace:workspace_user` | `awaken.runtime:workspace_user` | read Workspace configuration, model catalog, Files, Skills, Runs, and logs |
| Builder | `awaken.workspace:hosted_builder` | `awaken.runtime:workspace_admin` | Viewer plus create/edit/publish Agent configuration, Files and Skills, and create/resume/cancel Runs; no API-key, model-supply, membership, billing, or organization administration |
| Administrator | `awaken.workspace:hosted_admin` | `awaken.runtime:workspace_admin` | Builder plus Workspace API-key and hosted product administration; organization billing remains outside Awaken |

Builder and Administrator deliberately share the existing Runtime role because
both need the complete finite `run.*` lifecycle and Runtime currently contains
no administrator-only action. Creating a second equal Runtime role would
duplicate authority. Their least-privilege distinction is the Workspace role:
only `hosted_admin` receives `apikey.*`. Platform model supply stays read-only
for both; BYOK custody is administered through the hosted Workspace credential
surface, not by granting the self-hosted `model_supply.*` role.

The product profile is the sole permission source. Cloud may reference these
stable role identifiers while composing a user-visible access level, but it
must not copy the action matrix. Service roles (`publisher`,
`credential_ingress`, `agent_executor`, and `tunnel_manager`) remain separate
machine intents and are never human access levels or billable human seats.

Static structure:

```text
hosting subscription -- Builder capacity --\
                                       seat admission -> IAM role binding
Awaken access level -- product role bundle /
                                  |
                                  +-> awaken.workspace profile
                                  `-> awaken.runtime profile
```

Dynamic behavior:

```text
administrator selects Builder for one account and exact Workspace
  -> hosting checks one unique-human Builder capacity
  -> IAM atomically replaces the managed Awaken role family
  -> Workspace PEP admits authoring without credential administration
  -> Runtime PEP admits the existing Run lifecycle

buying capacity -> changes only the commercial ceiling
revoking Builder/Admin -> removes the role bundle and releases capacity
```

The product contract is tested as an exact-set decision table: Viewer has only
read authorities; Builder has Workspace/File/Skill write plus model read and no
API-key authority; Administrator adds API-key authority; every access level is
scope-local through the existing IAM binding and cannot cross a Workspace.

Implementation scope is explicit: the existing IAM binding store, Workspace
and Runtime profiles, Runtime administrator role, and PEPs are reused unchanged;
the existing Workspace profile compiler and Cloud role composition are
extended; the sole new product mechanism is the finite `hosted_builder` role.
No Builder subscription, Runtime-equivalent role, action evaluator, or
permission table outside the product profile is added.
