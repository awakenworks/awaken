# ADR-0099: Application-owned credential consumption

- Status: Accepted
- Date: 2026-10-28
- Supersedes: [ADR-0097](0097-protected-browser-authentication-boundary.md)
- Builds on [ADR-0098](0098-claimed-credential-delivery-revocation-fence.md)

## Context

Browser and Android applications need private credential consumption without
making platform runtime crates own site selectors or application login state.
This decision covers the generic delivery seam; production deployment remains
a separate qualification.

## Decision

Awaken owns generic credential custody, revision, access admission, recipient
envelopes and management IAM/audit. Applications own login plans, selectors,
browser/device runtimes, verification UI, identity checks and automation.
No browser login package is installed by Native. Browser login remains owned
by the independent application package `@awaken/protected-login`.

An explicit `AWAKEN_APPLICATION_CREDENTIAL_CONSUMERS_FILE` installs four POST
management routes under `/v1/config/application-credentials`: `enter`, `deliver`
and `check`, plus `execution` for live execution resolution ([ADR-0100](0100-credential-handling-and-execution-binding.md)). They retain the canonical management IAM/audit edge. POST calls
require the mapped workspace `apikey.write` authority. Each consumer additionally
admits exact canonical `authorized_actors`, workspace, credential keys, opaque
material types and a Workload holder. A missing identity is not anonymous
credential authorization. Browser, selector and Android semantics never enter
this platform registration.

Example non-secret registration (paths and identities are deployment-specific):

```json
[
  {
    "id": "growth-desk",
    "workspace_id": "configured-workspace",
    "handling_mode": "application_managed",
    "authorized_agents": ["configured-agent"],
    "authorized_actors": [{"type":"service_account_actor","service_account_id":"growth-private-executor"}],
    "holder": {"boundary":"workload","trust_domain":"growth.private"},
    "key_file": "C:/private/native/growth-recipient.json",
    "credential_keys": ["configured-account-reference"],
    "material_types": ["app.login/v1"]
  }
]
```

Entry accepts write-only structured fields and returns a native CredentialRef.
Rotation requires the existing revision. The encrypted material uses the
existing Vault and envelope implementation. The audit preserves call/tenant/
route/completion, but does not persist a material-derived body hash for entry.

Delivery binds the exact credential revision, consumer, opaque operation,
runtime epoch, plan digest, target object and recipient key identity. The full
context is authenticated inside AEAD plaintext; a public equality fingerprint
is not cryptographic authorization. The authority is rechecked after material
IO and before return. Applications call `check` again before physical use.
Installed key projections and consumer withdrawal are reread on each request.
The application decrypts its short-lived Workload envelope with its private
recipient projection. This does not enroll its material schema in the built-in
Worker plaintext resolver.

`enter` is a private ingress, never a model tool. The application accepts model
commands containing only account and operation references. API key use, browser
fill and Android node input are separate application consumers of this seam.

## Consequences

Applications must enforce private execution and prevent alternate observation
paths. Native retains credential custody and exact delivery admission.

## Qualification and limits

Windows E2E uses the real Native router, Vault handlers and recipient crypto,
with synthetic IAM/workspace and ephemeral repositories. Growth tests use its
real UI/MCP/coordinator and Chromium against explicit Flow/site fixtures. App
Control tests use its real private ingress/coordinator and an actual Windows
Android AVD running a synthetic login APK. These do not qualify persistent
production storage/IAM, real ChatGPT/Doubao/DeepSeek login, or real WeChat login.

Production deployments must isolate the executor key, Vault access, browser
profile/device transport and private UI from model filesystem/process tools.
Reference-only MCP is insufficient if the same model can read the recipient key
or issue unrestricted CDP/ADB commands. Secrets necessarily enter trusted
executor/target memory; JavaScript/Python strings are not guaranteed zeroizable.
No claim is made of universal login or bypass of provider human verification.
