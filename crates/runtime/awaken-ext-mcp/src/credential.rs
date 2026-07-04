//! Opaque credentials for authenticating to an MCP server.
//!
//! Re-exports the shared host-side credential vocabulary from
//! [`awaken_credential`] — the same seam the A2A outbound client uses, so the
//! challenge shape and refresh semantics cannot drift between wire clients.
//!
//! A [`Credential`] carries an already-resolved secret value (a bearer token or
//! a header pair) — never a vault reference or lookup policy. The host resolves
//! its secrets and hands this crate the opaque value, keeping credential
//! mechanics (vaults, OAuth refresh) out of the runtime/extension boundary
//! (D6/D9): this crate only attaches the value to a request.
//!
//! Rotation stays host-owned through two hooks on the HTTP transport: the host
//! can push a fresh value at any time
//! ([`set_credential`](crate::http::HttpTransport::set_credential)), and it can
//! register a [`CredentialRefresher`] that the transport calls when a server
//! answers 401/403 — the OAuth/vault machinery runs on the host side of that
//! callback, and this crate only retries with whatever value comes back.

pub use awaken_credential::{AuthChallenge, Credential, CredentialRefresher};
