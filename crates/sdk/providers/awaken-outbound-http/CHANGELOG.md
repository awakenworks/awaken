# Changelog

Notable changes to `awaken-outbound-http`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [1.1.0] - Unreleased

### Added

- `internal_http_client_builder`.

### Fixed

- Require `url` 2.5.9 in the workspace release graph for
  [RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592.html).
  An isolated-process regression covers deeply nested `blob:` origins with a
  bounded thread stack and the ordinary HTTP(S) origin contract.
