# ADR-0097: Protected browser login execution boundary

- Status: Superseded
- Superseded by: [ADR-0099](0099-application-owned-credential-consumption.md)
- Date: 2026-10-27
- Builds on: [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md), [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md)

## Context

GEO sampling and content distribution need consumer browser sessions. A business
Agent can plan and initiate login without reading usernames, passwords, OTPs or
browser storage. A separate model-driven login Agent would not strengthen the
boundary. Giving the same model arbitrary JavaScript, CDP, filesystem access or
unfiltered pixels from a credential-bearing page would defeat reference-only
credential tools.

## Decision

Proposed target; only the first implementation below is built.

The existing Session and its realized Environment remain the process/lifecycle
owners. An account is a product Resource, not another Environment or Session
registry. A browser driver is a trusted consumer inside that execution boundary;
it neither owns a business queue nor decides account permission or publication
approval. No browser backend tag or executable behavior is hidden in inert
Environment metadata.

The business Agent invokes a closed command containing only account reference,
exact login strategy revision and operation identity. The Native application
must derive its frozen account, CredentialRef, exact target/usage, plaintext
holder, Session binding and current authority independently of those arguments.
The existing Vault/materializer resolves the admitted credential through a
browser-specific consumer of the extension seam. A generic targetless Secret
mount is not an alternative for described browser credentials.

Versioned strategies compose bounded fill/click steps, HTTPS origins, form
targets, expected result URL and account-subject verification. The model may
propose a candidate strategy using a separate credential-free observation, but
that proposal cannot authorize a new credential destination or execute code.
This first component accepts only preselected strategies. Model-driven strategy
discovery and safe publication are not implemented.

The component supports bounded, exact semantic rules (native labels,
`aria-label`/`aria-labelledby`, roles/names, autocomplete, field names and
placeholders) plus native CSS. An optional unique form scope disambiguates login
from registration. An ambiguous visible match refuses input rather than
choosing a first match or a weaker fallback. Frames, shadow roots, the full ARIA
name algorithm and arbitrary model-supplied selectors are outside this slice.

Before filling, a secret-free selection prepares a private document/node
ticket. At physical input the browser synchronously reselects and checks the
same node, document, form, field type, visibility/editability, HTTPS origin and
same-origin POST destination before setting the value. It never validates one
node then calls a locator that may resolve another. Tickets do not survive
document replacement. Bounded relocation is allowed for read-only preparation
or an explicit no-write stale receipt; uncertain input is not retried. Current
Native authority is checked immediately before dispatch and after completion.
The component does not claim to make a remote authority check atomic with DOM
input; production must retain Native execution fencing and process ownership.

All secret-bearing steps execute privately. The model-facing login projection
contains only closed state/reason enums: no URL query, DOM, pixels, raw errors,
credential value or external account subject. Current authority is checked
before and after awaited I/O. Secret entry is serialized with other uses of the
same private browser. Failed/unknown attempts retain their original outcome;
repeating a command or changing its key never automatically submits again.

Login-only observation is not a general browser observation filter. The Native
composition must prevent alternate read/evaluate/CDP/shell/file paths to the
same page/profile, disable trace/HAR/video/screenshot capture before secret
entry, and enforce the allowed destinations below page JavaScript. A JavaScript
driver cannot establish OS isolation or a no-bypass network policy by itself.
Redirects and federation require explicit admitted origin transitions. The
platform necessarily receives credentials intended for it; this design does
not protect credentials from a compromised authorized destination.

Profile persistence is a private Native credential responsibility. It must be
encrypted, account-scoped and excluded from ordinary workspace, Memory,
Artifacts and exportable checkpoints. Rotation/reseal, final consumer fencing,
exact process stop, and upstream session/token revocation retain their existing
owners. Neither an expiring local capability nor closing a login panel revokes
an upstream cookie. Unsupported mandatory external verification returns a
machine-readable `action_required` result; the normal workflow is Agent-led.
The component distinguishes QR scan, SMS/email/authenticator/generic codes,
CAPTCHA, trusted-device approval and generic manual verification. Conservative
visible native-DOM signals and origin-scoped pinned provider rules detect these
gates before input, at the final guard, during origin transitions and after
submission. Hidden/alternative controls do not authorize a challenge. No raw
page evidence, QR ticket/image, code value or provider instruction reaches the
model. A fixed browser-safe notice catalog supplies user-facing copy and a
protected-verification UI intent. This does not install that UI or a resume
handler. A blocked transaction never auto-resubmits; a previously uncertain
write is not reclassified as safe to resume. Explicit private OTP consumers
retain their exact-field admission and never route values through a model.

## Consequences

`packages/browser-runtime/src/protected-login.mjs`, `login-policy.mjs`,
`login-target.mjs`, `login-dom.mjs` and `login-notices.mjs` implement the bounded
execution component over a privately supplied Page, current-authority port and
write-only credential-consumer port. It creates no browser process, stores no
credential and exposes no Page/tool escape. Its `observe` result is the last
login transaction state, not a fresh platform session-readiness probe.

The package is not wired into Native Session/Worker startup, Flow connectors or
Growth Desk. Its injected ports are not a production Vault or an IAM grant.
The existing Growth Node receiver still owns its development browsers and
cannot claim the target Native custody boundary. The following integration is
required before enabling this feature:

1. Real Session/Environment-owned driver launch, Worker capability admission,
   placement and lifecycle fencing through the existing execution composition.
2. Exact Credential extension consumer, private material transport and current
   authority checks at the physical input boundary; private Profile custody.
3. Mutually exclusive model observation/control and secret handling, with
   provider-enforced isolation and no alternative tool or process access.
4. Flow account/operation authorization and pinned strategy references, then
   SDK/UI projection. No administrative identity enters an App business path.
5. Three-layer E2E, process crash/recovery, revocation, unknown external effects,
   and separate per-platform live login qualification.

## Validation

Unit tests cover closed commands/results, foreign accounts, unknown strategies,
unsafe form targets, GET credential submission, current-authority loss before
and during filling, identity mismatch, concurrent calls, private error handling
and non-replay. Real local Windows Chromium tests cover same-page password,
two-step and federation pages plus dynamic IDs, semantic fallback, optional form
scope, hidden honeypots, deferred/editable fields, form redraw and detached-node
relocation. Race injection immediately before the private browser protocol
checks form action/owner changes, field type changes, ambiguity, document reload
and cross-origin navigation. Refused writes leave the password empty; uncertain
input cannot replay. All pages and credentials are synthetic; all browser
requests are intercepted. A local HTTP model fixture receives only the
command/status projection; username, password, OTP and Cookie canaries and
their URL/base64 encodings are absent.

Challenge witnesses cover each requirement type, outer frame metadata, pinned
provider rules, overlapping gates, hidden controls, alternative QR buttons,
exact admitted private OTP input, gates before password input and after submit,
pre-federation gates and lost input acknowledgement. Notice tests reject
arbitrary status strings and credential-bearing page evidence, retain the
closed code, require current authority and never replay blocked transactions.

These are component boundary witnesses. They do not prove a production model
transport, native process isolation, durable receipts/Profile custody, actual
provider federation, or a deployed three-layer business E2E.
