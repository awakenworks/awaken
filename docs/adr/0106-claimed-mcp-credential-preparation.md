# ADR-0106: Prepare Exact MCP Credentials Before Physical Run Admission

- Status: Accepted
- Date: 2026-11-11

## Context

Application-issued MCP credentials may expire while a durable Run waits in the
queue, awaits input, or recovers on another Worker. An application time bucket
is not an execution lease. Issuing a new credential after the Worker has already
materialized an acknowledged MCP generation creates another desired generation
without adopting it. Treating that desired pin as ready either bypasses the
native adoption boundary or repeatedly fails an otherwise executable attempt.

The existing credential rollout correctly waits while an ordinary Session is
busy. A new current Dispatch claim alone does not prove its predecessor stopped.
Credential preparation therefore needs a narrow native admission boundary,
while ordinary mutable Session commands retain their Idle rule.

## Decision

### Exact source facts and observation

The Credential owner retains expiry and application issuer generation as
secret-free facts of the exact source revision. Exact compilation copies those
facts into `CredentialAccess`; a later source read cannot reinterpret an older
pin. Changing bearer material or its expiry under the same application issuer
generation is rejected. A strictly newer generation uses the original source
mutation, managed-child, WAL, rollout and receipt owners. Native treats issuer
generation as opaque application identity; it does not derive policy from a
token's plaintext or decode a product JWT.

Missing expiry or issuer generation is historical unknown. In particular,
`None` does not prove an indefinitely valid credential. A consumer that needs an
expiry-qualified start requires the explicit, future expiry of the exact
selected access. Descriptor compilation, materialization and the existing
holder/target/revision checks remain separate gates.

Claiming a Run to prepare its Session is distinct from admitting a credential
effect. The MCP preparation compiler retains the original expiry while
validating its complete shape, policy, holder and envelope. It may acquire a
claim carrying an expired MCP pin so the canonical Session owner can replace
that pin. It does not authorize opening or using the expired material. Actual
MCP materialization rechecks expiry and exact source revision before an effect;
Model and Repository admission keep their existing stricter paths. Without
this distinction, an expired queued MCP pin cannot reach its own renewal owner.

The Worker projects `RuntimeRunContext.prepared_mcp` only from the actual active
MCP generations installed by the native realization owner. A requested desired
generation, Vault mutation receipt, or dispatch compatibility envelope cannot
manufacture this observation. The projection contains secret-free pins and is
not an authorization or renewal grant.

### Worker compatibility floor

An MCP access with either exact expiry or issuer generation requires
`session-mcp-credential-validity/v1`. The standard native Worker advertises this
capability; it describes compiled preparation and effect-admission semantics,
not application renewal or an authorization grant. Historical accesses with
neither fact preserve their original compatibility requirements.

One metadata helper derives this demand for Run freeze and run-free Session
preparation. Run compatibility validation derives it again from the canonical
published or legacy carrier, so removing a declared requirement cannot admit an
old Worker. A malformed legacy carrier is an invalid projection, never an empty
healthy capability set. Actual Worker assignment and claim checks enforce the
floor. This keeps new generic Workers eligible without requiring a composing
product's application capability. A generic Worker without that product's
preparer still refuses an expired materialization; it does not automatically
renew the credential.

### Borrowed claimed preparation

The authenticated Session-resume adapter first verifies the Worker, exact
Dispatch claim and current native Session Work receipt. It retains the original
`CommitEpochGuard` while exposing `ClaimedSessionCredentialPreparationContext`.
The context includes the original frozen activation, raw Run and Thread,
Session affinity, execution scope and Work receipt. It is not serialized or
installed in task-local or process-global state.

Every preparation mutation requires a live, uncancelled claim and an empty
physical-attempt slot from that same locked row. An exact occupied slot, another
claim's slot and corrupt slot all refuse. Lease expiry and Worker restart are
not predecessor-stop evidence; the original return/closure owner must first
establish physical quiescence.

For a non-Idle Session, the root additionally requires complete named activity
history and exactly one active activity belonging to the current raw Run and
Thread. A repair may use its parent Session but cannot use the parent's Run
identity. A peer activity is NotReady; missing history or foreign coordinates
are unverifiable and are explicitly rejected rather than retried forever.

The new Session update method accepts only a non-revoking CredentialLifecycle
update. The original normalizer and replacement validator preserve source,
target, holder, usage, origin and topology while advancing the exact pin
monotonically. Public fields, public MCP replacement and revocation retain
their ordinary admission paths. The ordinary `update_session` Idle guard is
unchanged.

### One desired state and realization path

The claimed update commits only desired state through the existing Session root
CAS. Requested, Realizing, Draining and Active without publication
acknowledgement already represent pending realization. Activity admission uses
this existing state to exclude new concurrent activities until exact
acknowledgement. Initial external preparation may still reserve its first Run;
an existing reservation cannot hide another activity. Root CAS orders a racing
activity admission against credential replacement. No new durable preparation
field, queue or scheduler is introduced.

The caller then invokes the original begin and Worker driver:
Requested → Realizing → Stage receipt → Activate → Publish/Drain → Acknowledge.
The existing generation, lease, receipt-set and publication guards remain in
force. `begin_claimed_session_realization` defaults to the ordinary begin for
neutral implementations; that default grants no credential mutation authority.
The native implementation rechecks the borrowed context before beginning.

Vault accepts an explicit rollout target for this one call. It uses the same
WAL, replay, rearm, reconciliation and complete-event acknowledgement as the
ordinary entry method, without changing the default delivery target.
`ClaimedCredentialRolloutAdoption` applies claimed admission only to the selected
Session. Other Sessions retain their ordinary rules; all targets must still
adopt before the full rollout converges.

Vault Pending alone cannot distinguish a desired-state commit from an update
failure. The borrowed target therefore exposes an in-memory typed outcome:
AlreadyAdopted or DesiredCommitted; otherwise NotReady, Conflict, NotFound,
Invalid or Unavailable. The application also reads back the exact desired pin
before continuing, then requires the actual active pin and expiry after native
realization. This outcome is neither stored authority nor a second receipt.

### Time and cancellation boundary

An expiry check establishes validity at admission, not a maximum Run duration.
Durable queue waits, approval without a deadline, retries, resumes and renewable
claim leases have no common finite wall-clock upper bound. A consumer must
revalidate at each executable admission. This decision does not promise
uninterrupted renewal inside a Running attempt or preservation of validity
through an unbounded human wait. A longer TTL is not a substitute for a native
deadline or a new admitted executable segment.

Cancellation uses its original exact control and cleanup authority. It grants
no execution and must not require issuing or adopting execution credentials.

## Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `ClaimedSessionCredentialPreparationContext` | Borrowed boundary port | Observations under the original held claim | Frozen activation, Work receipt and physical slot | Wire authority, stop inference, durable state | Stale or occupied claim authoring credentials | G66; Dispatch HTTP preparation table |
| `SessionUpdateCommands::update_claimed_session_credentials` | Application command | Credential-only root CAS under current activity proof | Canonical normalizer and replacement law | Public authoring, physical realization | Busy-peer or foreign-Run replacement | G66; Session contract and SQLite rollout tables |
| `ClaimedCredentialRolloutAdoption` | Per-call rollout adapter | Selected Session outcome of the existing adoption use case | Borrowed context, Session updates and publication | WAL, complete-event acknowledgement, global target replacement | Pending mistaken for a committed desired pin | G66; typed outcome and exact replay assertions |
| `PreparedMcpAttachment` | Runtime observation value | Secret-free actual active MCP access | Native installed generation projection | Desired-state authority or renewal | Desired v2 substituted for active v1 | G66; Host context and credential expiry tests |

## Alternatives rejected

- Renewing from an execution decorator after placement creates an unadopted
  desired generation and crosses the wrong authority boundary.
- Extending a TTL cannot bound queue, human-wait or restart duration.
- Allowing all busy credential updates weakens peer exclusion; a new claim or
  a process-local lock alone cannot prove physical quiescence.
- A separate preparation queue or stored readiness flag duplicates the Session
  MCP desired/active state and its existing recovery owner.

## Consequences

Credential, Session and Dispatch keep their original persistence and failure
authorities. The new preparation capability borrows existing exclusion and
adds no execution grant. Applications must distinguish desired-state progress
from actual adoption and retain explicit recovery for expired executable
admission. Unknown historical activity cannot be promoted to quiescence.

## Validation and qualification boundary

The following source tests are authored owning checks; execution qualification
is pending. G66 remains Target until the required owners are green together.
Deployment and real-provider acceptance remain separate.

| Behavior and negative control | Existing owning source |
|---|---|
| Same-generation material/expiry mutation refused; exact replay and strictly newer generation | [Vault application MCP expiry tests](../../crates/platform/credentials/awaken-credential-vault/src/repo/application_mcp/expiry_tests.rs) |
| Explicit per-call target keeps ordinary target and WAL/replay behavior | [Vault target tests](../../crates/platform/credentials/awaken-credential-vault/src/repo/application_mcp/expiry_tests.rs) |
| Generic expiry, exact material and issuer facts | [Credential expiry tests](../../crates/platform/credentials/awaken-credential-contract/src/expiry_tests.rs), [material expiry tests](../../crates/platform/credentials/awaken-credential-vault/src/local_material/expiry_tests.rs), [credential wire tests](../../crates/sdk/contracts/awaken-agent-contract/src/credential/wire_tests.rs) |
| Empty/current/other/corrupt physical slot, cancellation, expiry equality and frozen input | [Dispatch HTTP claimed preparation tests](../../crates/platform/dispatch/awaken-dispatch-http/src/worker_dispatch/claimed_preparation.rs) |
| Only current named activity; parent/repair mismatch; missing history; Requested/Realizing/Active-without-ACK blocks peers | [Session aggregate decision tables](../../crates/platform/session/awaken-session-contract/src/state/session_repo/controlled_aggregate_tests.rs) |
| Real SQLite Session and Dispatch, owned clock, root/repair positive controls, peer/foreign/history/occupied/expired refusal, desired commit through exact ACK, typed replay outcome | [Coordinator claimed preparation tests](../../crates/apps/awaken-coordinator/src/session_credential_rollout_tests/claimed_preparation_tests.rs) |
| Expiry-only, issuer-generation-only, both and historical unknown; actual old/new Worker assignment and claim; stripped demand and malformed legacy carrier | [Worker compatibility matrix](../../crates/platform/dispatch/awaken-worker-contract/src/requirements/mcp_validity_tests.rs), [Run projection and legacy carrier tests](../../crates/platform/dispatch/awaken-dispatch-contract/src/dispatch/tests/session_mcp_requirement_tests.rs) |
| Shared Run/run-free metadata demand; original run-free selector excludes old Worker without root mutation and admits new generic Worker | [Dispatch preparation mapper tests](../../crates/platform/execution/awaken-dispatch-execution/src/placement.rs), [Session initial preparation tests](../../crates/platform/session/awaken-session-application/src/tests/realization/initial_preparation.rs) |
| Actual installed active generation becomes the attempt observation | [Native Host MCP generation tests](../../crates/platform/execution/awaken-session-executor/src/host/tests/mcp_generation.rs) |

The SQLite phase test uses explicit physical MCP receipt fixtures and real root
and Dispatch persistence; it does not claim provider materialization, process
death or deployed product qualification. A composing product additionally needs
an actual Worker/MCP test covering a bucket boundary, delayed admission,
resume/restart after expiry, exact adoption and cancellation without renewal.
