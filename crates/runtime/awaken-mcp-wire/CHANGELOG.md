# Changelog

Notable changes to `awaken-mcp-wire`. Versions follow the [SDK versioning policy](../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`0.1.0`).

## [0.1.1] - Unreleased

### Added

- `http::execution_context_enabled`.
- `JsonRpcPeer` implements `Drop`: dropping the peer closes its connection, even while callbacks hold a notifier.
