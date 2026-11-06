# Changelog

Notable changes to `awaken-mcp-wire`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`0.1.0`).

## [0.2.0] - Unreleased

### Changed

- `McpTransportError::AuthenticationRequired` identifies a transport-authored authentication challenge. Server payload text remains a protocol error even when it imitates an authentication message. Exhaustive matches need the new arm; this remains inside the unpublished `0.2.0` change set.
- `McpTransportError::ToolFault(code, message)` carries a server tool fault with its JSON-RPC code instead of folding it into `ServerError` (ADR-0103). Exhaustive matches need the new arm.

### Added

- `http::execution_context_enabled`.
- `JsonRpcPeer` implements `Drop`: dropping the peer closes its connection, even while callbacks hold a notifier.
