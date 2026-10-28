# ADR-0098: Claimed credential delivery revocation fence

- Status: Accepted
- Date: 2026-10-27
- Amends: [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md)

## Context

Claimed materialization checked execution ownership before SecretStore I/O.
Revocation, owner replacement or authority unavailability during an awaited
read could still release material and record a realization receipt. The
existing ignored T15-C02 reproducer failed on the 74265bfc base when explicitly
executed on Windows/MSVC. A completed pre-read check is not current authority
after an awaited operation.

## Decision

Observe the original admitted read to completion, then verify attempt ownership
before recording a realization receipt. A lost or unavailable authority denies
delivery and creates no new receipt. Do not abandon or repeat the read. The
redacted material stays local to the failed call and is dropped.

Recording is another awaited boundary. Recheck ownership after the original
recorder settles and before returning material. A receipt already admitted
before revocation may remain as historical evidence; it grants no renewed
execution. Do not delete it or repeat the read to hide that history.

`PinnedCredentialMaterializer::materialize_claimed_material` is shared by
Provider (also used by ACP last-mile materialization) and remote delivery.
This introduces no new authority, credential selection, store, public API,
lease type or retry path.

The integration reuses main's common Provider/A2A implementation and its W0-W7
read/receipt barrier table, including delayed receipt acknowledgements.

## Consequences

This fence controls a new delivery; it cannot erase material already handed to
a consumer or revoke an upstream token. The final consumer still checks current
execution authority before spawn or external I/O and owns process stop and
provider-specific token revocation. Revalidation is not a distributed atomic
transaction with the upstream service.

The formerly ignored T15-C02 reproducer becomes an ordinary regression.
`tests::revocation_wait` places barriers inside the real materializer's Vault
SecretStore and recorder ports and verifies healthy delivery, revocation,
owner replacement and authority unavailability for both consumers. Before-read
denial performs no new read; during-read denial observes exactly one read and
creates no new receipt; during-record denial observes the original receipt but
returns no material. Cached-success replay remains a failing negative control.

The local Windows component run does not qualify every L3 native/provider facet,
a browser Profile, a browser credential adapter or an external login flow.
Session acceptance stays a subset until those independent witnesses exist.
