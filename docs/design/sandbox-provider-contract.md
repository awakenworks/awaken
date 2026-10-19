# Sandbox provider contract

`awaken-provisioning-contract::SandboxProvider` is the only physical Sandbox
supply port. The container-only `ContainerEnvironmentProvider` projection is
removed rather than retained as an alias or a second implementation. The
associated `Environment` role preserves what the consumer needs: shared
`Sandbox` for filesystem providers, shared `SessionSandbox` for container
providers and their existing attempt/checkpoint participants. Readiness-only
callers use `SandboxReadiness`; it reports capability and liveness and never
creates, adopts, disposes, or supplies a parallel provider authority.

Creation has one synchronous `claim_for_effect(spec, effect, intent)` edge.
Its `ClaimedEffect` retains responsibility before the first poll, for both
unattached capacity and an aggregate-fenced realization. `Rebuild` still needs
the existing aggregate fence and original source handle. Adoption carries the
same effective spec, handle and retained attempt in `SandboxAdoption`, plus the
explicit attached/unattached `SandboxEffect`. Existing backend methods retain
their current-authority, marker, projection, UID and resourceVersion checks.
Warm capacity can serve only unattached Create; fenced realization and Rebuild
continue through the exact underlying provider without a warm checkout.

Handle layout and closed-incarnation validation belong to the provider codec.
Session environment composition asks the one provider rather than interpreting
the same filesystem or container handle again. Filesystem absence may have no
incarnation; container absence, terminal and disposing evidence require the
original physical incarnation. These facts grant no disposal authority or
Session cleanup receipt. Restore capability is one `SandboxRestoration` value:
Unsupported, Targets, or Restorable. A Restorable rung always includes target
custody, so independent optional roles cannot advertise an inconsistent pair.

Kubernetes Create request/response wire values belong to the existing
`awaken-container-contract`. Moving those values changes their Rust import
path, not their serialized fields or the original ledger's identity and replay
law. Kubernetes package realization keeps one verifier, with bounded identity,
immutable projection and decoded-fact responsibilities in private modules;
retained-claim Pod cleanup remains the same durable interlock in a private
continuation module. Neither split introduces another scheduler, retry budget,
ledger, state machine, or physical cleanup authority.


The public Rust migration affects awaken-cloud's
`awaken-hosting-durable/src/container_continuation.rs`, its
`tests/provider_fixtures.rs`, and `restore_tests.rs`: replace the container-only
trait with `SandboxProvider<Environment = dyn SessionSandbox>`, implement its
`SandboxReadiness`, and supply the one `SandboxRestoration` rung. Vocabulary
uses `SandboxAdoptionLayout`, `SandboxAdoption`, `SandboxRealizationIntent` and
`SessionSandbox` from the provisioning contract. The Gateway's existing
allowlist capability and normalization imports do not change. This source
migration does not claim that awaken-cloud or a deployment has been qualified.


## Container failure value ownership (2026-10-18)

`awaken-container-contract::RuntimeError` owns the existing seven container
failure values, display text, idempotent `after_mutation` rule, shared Container
API status projection and the single conversion into provisioning
`SandboxError`. The contract already depends on provisioning; this move adds
no implementation edge or error protocol. Its direct `thiserror` declaration
is already in that dependency closure. Moving the existing `Backend(String)`
variant adds one measured contract catch-all; the canonical ratchet records
that relocation explicitly rather than disguising or renaming the variant.

The Docker-local `RuntimeFailure` and Kubernetes-local `KubernetesFailure`
traits still classify their own typed transport sources. Only typed backend
API responses use the shared `u16` status projection. Kubernetes applies the
existing quota predicate first; rendered mechanism text never supplies a
status. The Worker's outer broker HTTP failure remains uncertain and never
uses this inner Container API projection. Post-mutation failures preserve
capacity, expired-fence and already-uncertain classes; Provider activation
still chooses its original retention or same-effect fence refresh before the
port conversion. This movement changes no claim settlement, retry, ledger,
participant or cleanup authority, and does not remove Coordinator's concrete
implementation dependencies.

The Platform Rust path `awaken_sandbox_container::RuntimeError` is removed;
all repository callers import the canonical container contract directly,
without a compatibility alias or private forwarding path. The SDK public
surface is unchanged. The error has no serialized wire or persisted format.
The inspected current and continuation Cloud source trees have no
`RuntimeError` references or container/Kubernetes glob imports; that bounded
consumer review does not qualify unknown external Rust consumers or a Cloud
build or deployment.


## Shared Create facts and capture vocabulary (2026-10-18)

The existing container contract owns the Create request/ticket/result/status/
origin values, admission and join observations, borrowed K8s ledger binding,
reply profile/version/header, shared isolation metadata names and the original
metadata-field predicate. Container handle codecs, Kubernetes, Worker and
Coordinator import that one owner directly. Their former Container and K8s
public paths are removed without aliases. The inspected Cloud Rust roots have
no references to these symbols or implementation glob imports; unknown external
Platform consumers and deployments retain their own qualification boundary.
SDK public paths, JSON fields/defaults, ticket key bytes, wire version and
header markers do not change. The existing blake3 key is preserved; its new
direct contract declaration adds no closure node because provisioning already
reaches it through agent-contract.

These values hold no client, live activity or permission. Forward is an
observation returned by the original durable CAS, and Complete joins original
Create requests rather than proving participant disposal or Session cleanup.
The same physical ledger keeps private state, completion-source admission,
8 KiB result reserves, full ConfigMap capacity and UID/resourceVersion CAS.
Kubernetes keeps actual Client/Body capture, isolation and finalizer operations;
its direct-mode read/write guards remain absent from brokered admission. No
new port, Activity, permit, ledger, client or handle codec is introduced.

The existing profile retains the 4 MiB whole-response encoding law and complete
legacy scope. Broker GET non-200 still returns without polling the Body; GET
200 and every POST keep complete capture, original UID/nonce/digest checks and
the same Registry recheck. Complete outward bytes are encoded before finish
CAS and those same bytes are sent. A cancelled Worker HTTP response cannot
cancel the original detached request; unknown API/capture/finish outcomes
retain InFlight/Unknown under the original authority.


After all Coordinator ticket imports move to the contract, its unused direct
sandbox-container dependency is removed. The original concrete Kubernetes
ledger/client still retains the indirect container implementation path; this
single edge removal is not whole Coordinator implementation independence.


## Privileged Create Broker effects boundary (2026-10-19)

`awaken-container-contract::K8sCreateBrokerEffects` is the Coordinator Create
broker's caller role. The original `K8sWorkerCreateLedger` implements its eight
ledger observations/operations and two raw API effects directly. Their former
inherent bodies are moved, not kept as forwarding compatibility methods. The
CLI constructs one ledger Arc for Broker and its original binding/stop adapters;
Broker receives only that same role Arc, not a separately selectable client.

The original raw Client/Body/collector stays private to Kubernetes. GET non200
returns unobservable without polling the body; exact200 and every POST capture
the complete body under the original explicit profile. RawCreateFailurePhase
names only RequestBuild, Send or Capture, not no-effect/retry/cleanup authority.
It is not serialized. Coordinator keeps all path/JSON/current Registry/nonce/
UID/digest/definitive-result/Seal decisions and their exact outward errors. The
original detached lifetime, ConfigMap UID/resourceVersion CAS, nonce-once,
capacity, isolation/finalizer and finish-after-complete-encoding laws stay put.
Only the existing collector remains, with its dead public export removed.

Platform callers of the moved methods import K8sCreateBrokerEffects directly
from its contract; K8sCreateBroker::new takes one effects Arc plus the original
namespace/provider inputs. Registration/process retention and provider-private
isolation methods remain inherent. Worker K8sCreateTransport is the opposite
signed remote direction and gains no privileged role or RBAC permission. The
current inspected Cloud Rust roots have no direct Broker/ledger/collector calls;
that bounded source observation does not qualify unknown external consumers.

Coordinator removes the two concrete normal dependencies but keeps them as
explicit dev dependencies for its original real client/ledger regressions. The
shared CLI still assembles Kubernetes and Worker, and other Coordinator paths
still reach executor-side implementations. This change does not claim zero
component-total/binary closure, physical cleanup or a Session cleanup receipt.
SDK release types, source and snapshots are outside this migration. Final
qualification and the normal Cargo-generated lock must bind the actual received
source rather than these prepared source changes.
