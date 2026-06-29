# Credentials, Vaults, And Availability

Credential lifecycle is a credential-domain/product concern. Runtime code should see
only opaque references and explicit permission decisions.

## Bounded Context Split

| Context | Owns |
|---|---|
| Runtime Core | typed tool calls, permission hooks, opaque credential references |
| Dispatch / Server | passing resolved references into runtime activation when required |
| Credential Domain / Product | vault schema, credential CRUD, OAuth refresh, account grouping, availability projection, operator UX |
| Orchestration layer above | credential delivery into tool execution (out of scope here) |

## Domain Model

| DDD type | Name | Rule |
|---|---|---|
| Entity | `CredentialRecord` | Stores identity and metadata, not public grants |
| Entity | `Account` | Groups credentials for one provider principal |
| Value object | `CredentialRef` | Opaque outside the credential context |
| Domain service | credential selector | Chooses an eligible candidate, never authorizes |
| Domain service | refresh coordinator | Single-flight refresh and atomic write-back |
| Projection | availability state | Operational status, not access control |

## Runtime Boundary

Runtime inputs may include an opaque `CredentialRef` or policy decision. Runtime
must not know:

- vault table layout;
- OAuth grant/refresh schema;
- tenant sharing rules;
- provider billing quota;
- public credential API shape.

Tool execution asks the permission path whether use is allowed. The credential
context supplies material only after authorization.

## Selection Is Not Authorization

Selection answers "which candidate can be used if policy allows it?" It never
answers "is the caller allowed?"

Availability checks follow the same rule:

- success may clear a cooldown/login-required projection;
- failure or unknown never marks a credential available;
- probe result carries no grant;
- disabling/enabling is an operator action in the product context.

## First Vertical Slice

For product-owned credential work, build slices in this order:

1. secret-in/secret-free-out credential CRUD;
2. opaque `CredentialRef` resolution for one provider;
3. explicit permission check before materialization;
4. single-flight refresh if the provider uses rotating credentials;
5. availability projection and manual check endpoint.

Do not implement credential pools, account spreading, or quota routing until the
single-provider flow is working.

## Non-Goals

- No vault schemas in runtime crates.
- No grant field on selector, probe, or capability results.
- No ambient environment fallback for missing credential refs.
- No product policy in store/repository implementations.

## Guardrails

G8 and G9 in [INVARIANTS](../INVARIANTS.md).
