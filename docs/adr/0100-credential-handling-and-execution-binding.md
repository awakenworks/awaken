# ADR-0100: Credential handling and live execution binding

- Status: Accepted
- Date: 2026-10-28
- Extends [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md) and [ADR-0099](0099-application-owned-credential-consumption.md)

## Context

Platform integrations and application executors consume the same credential
mechanism but own different plaintext boundaries. Application consumption must
also end when its original Session, Agent or physical execution loses authority.

## Decision

`CredentialExecutionPolicy.handling_mode` has two closed values:

| Value | Owner after delivery | Existing constraints |
|---|---|---|
| `platform_managed` | Installed platform provider, MCP or other typed consumer | Exact holder, usage, target, source, exposure and realization admission |
| `application_managed` | Registered external application executor | Extension usage, Workload holder, forbidden model exposure, installed consumer and material type |

Handling is responsibility, not custody, a new permission or executable code.
Both modes reuse Vault entry/rotation, CredentialAccess admission and recipient
envelopes. Standard MCP Bearer authentication stays platform managed. Old wire
policies default to that mode and omit its default field on serialization, so
retained snapshot fingerprints and envelope AAD remain compatible. An explicit
application mode is authenticated in the published policy and envelope AAD.

The external-consumer registry requires `handling_mode` and `authorized_agents`
beside its existing canonical actor, workspace, holder, key and material types.
Changing a request field cannot override the registered mode. Registry changes
are reread at use time. The application executor is still responsible for user
authorization, selectors, input, challenges, identity and observation exclusion.

## Execution authority

`CredentialExecutionBinding` contains secret-free coordinates:

```text
session_id, agent_id, environment_id, run_id,
claim_owner, claim_epoch, execution_ref
```

These values are not independent credentials. `CredentialExecutionAuthority`
is a read port over the existing Session repository and original Dispatch
claim/physical attempt. It introduces no second grant, Session or lease store.
The Coordinator adapter verifies workspace ownership, a live visible frozen
Session, dispatch origin and Agent, execution scope, a current claim and the
exact occupied physical execution. Session revision and claim are reread to
reject observations invalidated during those reads.

All-in-one startup binds that port to its original Session/Dispatch owners
before serving Control. Split deployments must install an equivalent authority
adapter; an absent adapter refuses execution resolution and delivery. It never
trusts caller-supplied Session/Agent strings as an alternative.

## Protocol

1. The private application ingress calls `enter` with structured fields. Only
   `CredentialRef{id,revision}` is returned. This does not require a running
   Agent and does not expose secret fields to MCP.
2. Native HTTP MCP transport adds the task-local runtime Run ID as
   `x-awaken-run-id`. Static configuration cannot override it. Model arguments
   remain `{account_ref, operation_id}`. This is correlation, not authorization.
3. The authenticated private executor calls
   `POST /v1/config/application-credentials/execution` with consumer, credential
   key and Run ID. Optional Session ID is a consistency assertion. Native
   derives the original parent Session and actual Agent/claim/physical binding.
4. `deliver` includes that full execution plus exact credential revision,
   operation, application epoch, plan digest, opaque target and recipient key.
   Native verifies current authority before/after material IO and before return.
   Full request context is encrypted inside the recipient envelope.
5. The generic JS/Python SDK verifies policy, recipient and encrypted context,
   calls `check` and exposes a short-lived field callback only to private app
   code. It checks expiry and current authority again before every field write.
6. The app validates target origin/APK, elements and runtime lease, executes its
   fixed script and retains the original receipt. Challenges use a separate
   private human ingress; model tools receive only status/reference projections.
7. Cancellation, claim takeover or Session termination prevents further use.
   An application may bind a new current attempt under the same Session, Agent
   and environment to continue verification or reconcile identity. It retains
   authority history and never replays the original password submission.

The developer override accepts only an already existing Run selector; it cannot
create authority. External ACP now supplies runtime-owned correlation on every
new/load/recovery step through explicitly opted-in application MCP endpoints;
see [ADR-0101](0101-acp-application-execution-context.md). Missing correlation
still has no mutable Session-wide or model-authored fallback.

## Consequences

No separate credential execution registry is introduced. Applications use the
canonical live execution authority and retain responsibility for private input,
verification, identity and observation controls. Windows qualification and
production account qualification remain separate.

## Qualification boundaries

The Windows fixture composes real SQLite Session roots, Dispatch claim and
physical slot, Coordinator authority, Native router/Vault and recipient crypto.
IAM/workspace and target sites are explicit synthetic fixtures. Growth uses its
production BFF/MCP/UI and real Chromium. App Control additionally runs its fixed
helper on an owned Windows Android AVD with a synthetic login APK. No real
ChatGPT, Doubao, DeepSeek, GitHub or WeChat account is implied by these tests.

Execution binding limits when the broker allows use; it is not hardware process
attestation, remote memory erasure or rollback of already submitted effects.
There is a bounded race between a liveness check and an external side effect.
Applications must isolate executor keys, process memory, browser profiles and
device channels from model tools. Managed-language secret strings cannot be
guaranteed zeroizable; the SDK callback is a usage boundary, not a promise that
malicious application code cannot retain plaintext.
