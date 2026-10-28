# ADR-0101: ACP application execution correlation

- Status: Accepted
- Date: 2026-10-28
- Extends [ADR-0100](0100-credential-handling-and-execution-binding.md)

## Context

An external ACP process has an opaque protocol session ID. That ID cannot
identify the canonical Native Session or authorize application credentials.
Execution metadata also must not be sent to unrelated MCP destinations.

## Decision

The external ACP executor creates `McpExecutionContext` from the immutable
activation's Run and root Agent on every step. The protocol sends it with the
exact MCP projection on `session/new`, `session/load`, and missing-session
recovery. It never enters the prompt, model arguments, checkpoint or credential
material. The external ACP session ID is never treated as a Native Session ID.

Delivery is default-off. Only a connection explicitly authored with
`?awaken_execution_context=run-agent-v1` receives `x-awaken-run-id` and
`x-awaken-agent-id`. Native HTTP MCP uses the same opt-in rule for Run metadata.
The marker is frozen in the authorized endpoint URL, including its credential
target identity. Duplicate/unknown markers, userinfo and fragments cannot opt
in. Stdio and ordinary external services receive no execution identifiers.
One shared MCP wire predicate owns this selection; adding support to an
application does not authorize every configured MCP destination.
The Run, Agent and Session header names are reserved. Static headers and custom
authentication headers cannot supply or override them, even without current
execution context. Ordinary bearer/API-key authentication retains its behavior.

The application authenticates its normal caller and forwards the Run selector
to Native's existing `/execution` authority. Native resolves the original
parent Session, actual Agent, environment, current claim and physical execution.
The SDK also compares the ACP Agent assertion with that canonical result.
Headers are correlation, not grants; delivery still requires the registered
consumer, canonical actor/workspace, credential key/revision, holder, recipient,
handling policy and live execution. Every field write revalidates these facts.

Applications own login and verification. A new current execution may continue
verification under the same Session/Agent/environment without replaying the
original login. The SDK does not infer a Session from an opaque ACP session ID
or accept model-supplied execution coordinates.

Windows E2E uses the production ACP executor, protocol codec and owned child
process running a deterministic external ACP peer. That peer obtains an explicit
native MCP permission decision, calls the real Growth/App Control MCP endpoint,
and records model-visible request/result separately from private transport
configuration. Tests include private OTP and identity verification, real
Chromium and the fixed helper on a synthetic Android AVD. This qualifies the
wire/consumer chain, not real provider accounts or every third-party CLI build.
The native transport variant uses the production HTTP MCP client inside the
runtime's task-local tool context and runs the same application UI/device loop.
The two standalone qualification entrypoints are explicit composition roots.
The credential route and fixture helpers receive the existing process clock;
neither chooses an ambient clock. The clock gate also rejects a factory moved
from those roots into a helper.

The remaining open management/private request bodies are closed with Serde
`deny_unknown_fields`; the open-body ratchet is tightened to zero. Existing
typed clients remain the valid-path witnesses. A cooldown field typo is rejected
at the real HTTP extractor instead of silently selecting a default window.

## Consequences

Native and ACP transports share explicit endpoint admission and the same
credential execution authority. Missing context fails closed. Qualification
uses Windows protocol and application E2E; a real provider build and real
third-party accounts require their own deployment acceptance.
