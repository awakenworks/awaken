# Environment Sandbox timing UI cause-effect model

## Cause graph

```text
C1 placement = self_hosted
C2 Sandbox creation = on_tool_use
C3 Environment create succeeds
C4 policy create succeeds
C5 exact policy bind succeeds

!C1 ---------------------------> E1 hide native timing control; reject forced lazy input
C1 & !C2 ----------------------> E2 create Environment with implicit eager policy
C1 & C2 & C3 -----------------> E3 create version 1 on_tool_use policy
E3 & C4 -----------------------> E4 bind exact policy version to Environment
E4 & C5 -----------------------> E5 row and binding API report first Hand tool
E3 & (!C4 | !C5) -------------> E6 show failure; never claim deferred configuration
```

`sandbox_provisioning` is deliberately not inserted into the official
`EnvironmentConfig` union. The Console composes the independent policy and exact
Environment binding into one user workflow while the API retains their distinct
ownership and versioning.

## Decision table

| Rule | Placement | Timing | Expected writes | Visible result | Test |
|---|---|---|---|---|---|
| U1 | cloud | eager | Environment only | native timing control hidden | `buildDeferredSandboxPolicy` table |
| U2 | cloud | requested lazy | no policy | fail closed | `buildDeferredSandboxPolicy` table |
| U3 | self-hosted | eager | Environment only | eager | `buildDeferredSandboxPolicy` table |
| U4 | self-hosted | on tool use | Environment, policy v1, exact bind | first Hand tool | `Environment: configure native Sandbox creation on the first Hand tool` |

The backend binding projection includes `provisioning`, so the displayed value is
rehydrated authority rather than transient form state.
