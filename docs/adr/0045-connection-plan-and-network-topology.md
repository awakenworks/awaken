# ADR-0045: Connection Plan — Network Topology as a Value Object

- Status: Proposed
- Date: 2026-07-08
- Depends on: [ADR-0044](0044-remote-hand-tool-executor-over-a-channel.md) (the
  first consumer that needs a channel), foundation crate `awaken-connection`
  (already a transitive dependency of this workspace)
- Relates to: [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (only resolved secrets cross into execution; the plan carries a `CredentialRef`,
  not material)
- Prior art: awaken-next `awaken-connection-plan` — the policy layer this ADR
  ports/trims. Its mechanism layer (`awaken-connection*`) is the same
  `awakenworks/awaken-foundation` this workspace already pulls.

## Context

A remote hand (ADR-0044) and a remote brain (ACP) both need "a channel to the
other end." The two ends can meet in several topologies, and today the topology is
chosen **implicitly** by which provider/source the host happens to wire — there is
no value object that names *how* the two ends meet.

The mechanism to establish a channel already exists upstream and is already a
dependency of this workspace (`awaken-foundation`, rev pinned in
`Cargo.toml`):

- `awaken-connection` — `Channel`, `Transport` (typed `dial` with an `Address`
  and **opaque** `HandshakeMaterial`), `Dialer`/`ListenEnd`, `bind_pair`,
  `ConnectError`. Its crate doc is explicit: *"Policy-layer connection plans,
  authorization, handshake-material resolution, capability scopes … live outside
  this crate."*
- `awaken-connection-transports` — concrete `HttpTransport`, `NatsTransport`.
- `awaken-connection-auth` — `HeaderAuthMaterial` (`bearer(...)`), header safety.

So the topology work is **not** "build a transport layer." The mechanism is done.
What is missing is exactly the one layer foundation excludes on purpose: the
Awaken **connection plan** — a data value that names transport address, wiring,
dial direction, and a credential *reference*, and a factory that turns a plan into
a live foundation `Channel`.

## Decision

### D1: Adopt foundation's connection stack; do not reinvent a transport layer

`awaken-tool-relay` (ADR-0044) and the ACP brain channel are written against
`awaken_connection::Channel` / `Transport` / `bind_pair`. This workspace adds
direct dependencies on `awaken-connection`, `awaken-connection-transports`, and
`awaken-connection-auth` at the git rev already present transitively. No new
transport crate is created here.

Consequence: the `Direct` (remote, over `HttpTransport`) and `Relay` (over
`NatsTransport`) topologies are **transport-covered today**. Only the two local
arms need work — see D5.

### D2: Topology is one value object, `ConnectionPlan`

Ported and trimmed from awaken-next's `awaken-connection-plan`:

```text
ConnectionPlan {
    transport:  DialAddr,             // where
    wiring:     Wiring,               // direct, or via a broker
    dial:       DialPolicy,           // who initiates
    credential: Option<CredentialRef>,// which secret (a reference, never material)
}
DialAddr   = InProcess | Unix(path) | Tcp(addr) | Http(url) | Nats { url, inbox, outbox }
Wiring     = Direct | Relay { broker }
DialPolicy = Dial | Listen | ViaBroker
```

The four topologies are expressions of these axes, not a fifth enum:

| Topology | Plan |
|---|---|
| **InProcess** (degenerate) | `InProcess` + `Direct` + `Dial` |
| **Direct** (brain dials hand; same host `Unix`, remote `Tcp`/`Http`) | `Unix`/`Tcp`/`Http` + `Direct` + `Dial` |
| **Reverse** (hand dials out, NAT; brain listens) | `Tcp`/`Http` + `Direct` + `Listen` |
| **Relay** (both meet at a broker) | `Nats` + `Relay{broker}` + `ViaBroker` |

`ConnectionPlan` is a serializable value object with no product-hosting vocabulary
and no resolved secret (G16, G8): it is safe to log, persist, and carry across the
config-to-host edge.

### D3: One `ChannelFactory` maps a plan to a live channel; InProcess is degenerate

```text
ChannelFactory::connect(&ConnectionPlan) -> Result<Box<dyn Channel>, ConnectError>
```

The factory selects the matching foundation `Transport`, resolves
`HandshakeMaterial` from `credential` (D4), and dials or `bind_pair`s per
`dial`. The `InProcess` arm returns an in-memory duplex with **no serialization**
and no transport at all — the zero-cost degenerate case. Because every consumer
(remote hand, remote brain) takes a `Channel`, the **same consumer code runs from
a laptop (`InProcess`/`Unix`) to a fleet (`Tcp`/`Nats`)**. This is the simple-
design rule already stated in `neutral-waist.md`: keep in-process, server-hosted,
and out-of-process execution on one path.

### D4: The plan carries a `CredentialRef`, never resolved material

Following ADR-0043's boundary, `ConnectionPlan.credential` is a `CredentialRef`
only. `ChannelFactory` resolves it to an opaque `awaken-connection-auth`
`HandshakeMaterial` (e.g. `HeaderAuthMaterial::bearer`) **in the host**, via the
existing `awaken-credential-vault`, immediately before dialing. Resolved material
never lives in the plan, never serializes, never logs. A loopback `InProcess`/
`Unix` plan uses no credential. This reuses `connection-auth`'s bearer material
instead of inventing a bespoke proof (contrast awaken-next's HMAC
`LeaseCallbackProof`).

### D5: Missing local transports are contributed upstream, not forked

`awaken-connection-transports` today ships `http` + `nats`. The two local arms
this ADR needs — an in-process duplex and a Unix-domain-socket transport (the core
crate already models `Address::Unix` in its typed-pairing tests) — are added to
foundation `awaken-connection-transports`, keeping one connection family across
both repos. This workspace does not maintain a parallel local transport.

### D6: `AgentChannel` converges onto foundation `Channel`

`awaken-agent-channel::AgentChannel` (`AsyncRead + AsyncWrite + Unpin + Send`)
becomes a **use-site bound over `awaken_connection::Channel`**, not a parallel
marker. Foundation's `Channel` is deliberately a bare `Send + 'static` marker so
"protocols add their own bounds at their use sites"; the tool-relay and ACP use
sites add the `AsyncRead + AsyncWrite` bound they need. The ACP brain channel and
the ADR-0044 hand channel then sit on one mechanism.

## Development-Ready Design (G14)

| Required item | This ADR |
|---|---|
| Bounded context | **Neutral Platform** (README: "reusable connection and control mechanisms, free of product DTOs") |
| Model element | Value objects `ConnectionPlan`, `DialAddr`, `Wiring`, `DialPolicy`; reuses foundation `Channel`/`Transport` and `connection-auth` material |
| Port / repository | `ChannelFactory` (plan → `Channel`); `CredentialResolver` (ref → `HandshakeMaterial`, backed by `awaken-credential-vault`) |
| Owning crate | new `awaken-connection-plan` (Neutral Platform), depending on foundation `awaken-connection*`; local transports contributed to foundation `-transports` |
| Guardrail + enforcer | **G34** (new): `ConnectionPlan` and the plan crate carry no product-hosting vocabulary and no resolved secret — only `CredentialRef`. `lefthook.yml` vocabulary deny-list + no-secret-serialization test + `deny.toml` (plan crate must not depend on runtime core) |
| First vertical slice | `ChannelFactory` with `InProcess` + `Unix` arms; ADR-0044's `RemoteToolExecutor` takes a `ConnectionPlan` instead of a hardcoded channel; a `bearer`-authed `Unix` plan resolves its `CredentialRef` through the vault; a no-secret-serialization test asserts material never appears in the serialized plan |

## Consequences

- Topology becomes declarative and testable: a run's placement is a serializable
  `ConnectionPlan`, not implicit wiring.
- `Direct`/`Relay` come "for free" from foundation transports; only local arms are
  new, and they land upstream where they belong.
- New guardrail **G34** is added to [INVARIANTS.md](../INVARIANTS.md) as **Active**
  with the plan crate.
- `awaken-agent-channel` loses its parallel marker (D6), reducing two channel
  abstractions to one.
- The plan is orthogonal to ADR-0044's executor and to the placement ADR: the
  three axes (what runs where / how ends meet / where the process lives) never
  reference each other's types (ADR-0034 axis separation, D18).

## References

- foundation `awaken-connection` — mechanism layer, and its "policy lives outside"
  boundary this ADR fills.
- awaken-next `awaken-connection-plan` — the ported reference for `ConnectionPlan`.
- [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  — resolved-secret boundary the `CredentialRef` respects.
- [neutral-waist.md](../design/neutral-waist.md) — "one path" simple-design rule.
- [INVARIANTS.md](../INVARIANTS.md) — G8 (opaque secret refs), G16 (neutral
  vocabulary), G34 (new: plan carries refs, not material).
