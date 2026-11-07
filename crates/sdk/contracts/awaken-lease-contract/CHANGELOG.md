# Changelog

Notable changes to `awaken-lease-contract`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [1.1.0] - Unreleased

### Added

- `Epoch::successor` and `EpochExhausted`: the one first-grant-or-next law over a lease generation, and the one classification of its exhaustion.
- Authority renewal decisions that let a request run up to the remaining proof and retry the initial heartbeat within its proof window: `RenewalDecision::Apply`, and `AuthorityEnd::{AuthorityLost, ProofExpired}` naming why authority ended.
