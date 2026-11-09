# Agent catalog and personal-selection governance test design

The Config owner stores definitions, publications and personal preferences. IAM
alone decides current identity, action and resource authority. This design
specifies qualification required by the native directory and personal-selection
HTTP boundary; it does not
claim that Studio review provenance or the Desk interface is implemented.

## Required behavior

- Only a current native direct Account can manage personal preferences or shared
  definitions. A Service, delegated actor, API key, channel message proof, UI role
  label or model tool context cannot stand in for a person.
- Directory manifests require current `agent.use`; complete definitions require
  `agent.definition.read`. Personal selection read/write are separate actions.
  Choosing an Agent grants none of these permissions.
- Exact registered-resource evaluation must qualify namespace, action, product,
  type, issuer/workspace-bound resource identity and owned native projection in
  the same native current fence. Missing deployment product metadata is explicit
  not-configured; existing profiles are not silently adopted or granted roles.
- The ordinary Workspace evaluation's freshness is only a consistency observation.
  Its Allow, Deny and approval decision never grant an object operation.
- The existing Config adapters provide a bounded single-snapshot read and a
  monotonically increasing scope epoch. Insert, update, delete/reinsert and
  data-only ABA participate in the same transaction. Publication observations follow
  the actual execution Workspace so unrelated execution publications retain their
  independent commit locks. Missing epoch with data,
  corruption, overflow and dependency faults fail closed. Publication row index,
  declared state and content-addressed identity must agree with the decoded record;
  a mismatched row cannot become another Agent's usable manifest.
- Every object permission observation and the ending fence must match the start.
  Current person, trusted Workspace and catalog epoch are checked again before
  exposing data. No previous Allow or hidden manifest is reused after failure.
- Authorization filters precede pagination. Hidden candidates cannot change a
  healthy visible page, cursor or termination. No company-wide total is invented.
- A denied selection read is unknown, not false. A selected-only query refuses an
  unknown state rather than silently excluding it. Selected results describe the
  currently visible selected subset.
- Original receipt read and deselection do not require current Agent use. They
  still require the actual owner's current respective native action and expose
  no hidden name, manifest, definition or other person's preference.
- The original key/body/generation and atomic preference/receipt invariants remain
  those of [personal selection](personal-agent-selection-testing.md).

## Finite wire and explicit adoption

`GET /v1/agent-directory` accepts `limit` from 1 through 100, an optional opaque
`after` cursor and optional `selected=true`. It returns `data`, `next_page` and
`visibility=current-authorized-published-subset`. Each item contains only `id`,
`name`, exact published `version`, `type=agent` and a selection state. A known
selection carries its preference generation; unknown carries no invented
baseline. There is no global total. A native read has an explicit capacity of
4096 current Agent heads; exceeding it is unavailable rather than a truncated
successful directory. Permission filtering precedes visible pagination.

`PUT /v1/me/agent-selections/{id}` accepts only `request_key`,
`expected_generation` and `selected`. The caller supplies no owner, issuer,
Workspace, role or grant. `GET /v1/me/agent-selections/{id}/outcomes/{key}` returns
an observed original receipt or `state=not-observed`. Absence does not prove a
prior unknown attempt did not occur, and cannot authorize a replacement key.
Trusted routing may supply `RequestTenancy` or the original native
`WorkspaceScope`; neither a missing context nor a request body selects a fallback
installation/company. Both routes require the actual current owner and their independent native action;
only new selection also requires current use and an executable publication.

The deployment file may explicitly declare `agent_governance.product_id`. The
native product's exact Agent vocabulary and the Workspace PAP candidate are
separate declarations: wildcard legacy profile patterns are not product
vocabulary. The declaration helpers perform no registration, activation,
projection or grant writes. Missing configuration is unavailable. Native IAM
remains responsible for validating the product namespace and immutable profile
owner before activation; a supplied product id is never evidence of permission.
Resource identity uses length-prefixed verified issuer, trusted Workspace and
Config Agent id with a versioned digest. Labels and path separators cannot alias
another owner coordinate.

## Case table

| Case | Causes | Required effect | Executable owner |
| --- | --- | --- | --- |
| C1 | First publication, newer draft, newer publication for another execution Workspace | Exact older executable source; no newer draft name; independent execution publication locks | Config store `catalog_snapshot` conformance and publication-lock integration tests |
| C2 | Account/issuer/Workspace differ; same Agent ids | Only exact owner preference and exact scope data; no cross-owner default | Config store shared catalog conformance and native HTTP matrix |
| C3 | Insert/update/delete and same-value restoration; transaction rollback; absent or overflowing epoch | Monotonic committed observation; rollback preserves rows and epoch; invalid state unavailable; mismatched publication state/index/fingerprint/id rejected | SQLite/PostgreSQL catalog epoch and corruption tests and schema-upgrade test |
| C4 | Current direct Account versus Service, actor, API key, wrong issuer/audience, expired/future credential | Rejected identity reaches neither fine PDP nor preference write | Native HTTP matrix, signed by the native test authority |
| C5 | Exact Resource use grant without Workspace read; hidden candidates added or changed | Published whitelist only; same healthy page, cursor and termination; no hidden names/totals | Native HTTP matrix |
| C6 | Owner selection read denied; selected-only query | Market state unknown; selected-only query and original receipt read denied, never false/not-found | Native HTTP matrix |
| C7 | Duplicate confirmed key; remove; replay old select; withdraw use while keeping owner actions | One preference effect, immutable receipt, removal retained; minimal outcome and deselect remain independent of use | Personal-selection conformance and native HTTP matrix |
| C8 | Native strict 503, one failed ordinary fence observation, missing registration proof or freshness | Whole request unavailable; one failed current HTTP attempt is not retried into success | Native HTTP matrix with fault injection over actual native routes |
| C9 | Native PolicyAdmin changes the shared authorization fence during a page; Config changes during the same page | Discard the entire captured page, never combine older Allow with newer data | Native HTTP matrix, separately mutating IAM and Config owners |
| C10 | Missing declaration/trusted context; invalid bound/cursor; caller injects owner/range fields | Explicit unavailable/denied/invalid response and no preference state | Native HTTP matrix |
| C11 | Native SQL commit followed by withdrawn current identity, lost acknowledgement or bounded acknowledgement timeout | Unknown 503 without old receipt; one write attempt; restored owner GET reads the exact original receipt and never repeats PUT | Native HTTP matrix with a test-only repository reply decorator after the real SQL transaction |

The native HTTP matrix runs the same transcript against actual SQLite and required
PostgreSQL Config/IAM owners. Its signed-token fixtures qualify native credential
verification and live UserInfo, not a browser PKCE journey or deployed acceptance.
The original-key store tests remain authoritative for concurrent CAS and atomic
preference/receipt failure. The acknowledgement-loss/timeout decorator injects a
failure only after the actual SQL owner has returned its committed receipt; it
qualifies application unknown-outcome handling, not a database network fault.
Pre-write identity/permission rejection remains unauthorized/forbidden. After a
persistent attempt, an ending identity/permission guard failure is unknown 503;
it cannot relabel the attempt as definitely uncommitted or disclose an old
receipt. Recovery requires an independent current-owner GET of the same key,
never a second PUT masquerading as a read.

## Executable matrix

Cover both actual SQLite and required PostgreSQL adapters: consistent exact
published revisions, newer draft names not leaking, rollback, overflow, unrelated
scope isolation, concurrent writes, data-only ABA and delete/reinsert. A controlled
missing epoch-trigger mutation must defeat the ABA assertion before exact restore.

Native HTTP qualification registers an explicit test product vocabulary, profile
and owned projection. Test exact grants without workspace inheritance, wrong
product/workspace/issuer, absent or retired registration, wildcard legacy grant,
Account/Service/actor/API-key distinctions, same-token use withdrawal, remaining
owner outcome/deselect, denied owner read, outage and mid-page epoch drift. Native
dependency failure is 503, never a denied item skipped from a successful page.

Enumerate directory, Managed collection/detail/version, Config collection/detail/
version, publication/export, preview, hosted tools and known Session/Thread Agent
fields in GET/history/events/SSE. Known structural projections preserve message
content, tool outputs and citations; arbitrary nested keys are not recursively
scrubbed. Definition audit permission grants no personal conversation read.

Pure tests, real adapters, native IAM/HTTP, full repository hooks and deployed
browser journeys remain distinct claims. A skipped fixture or unsupported old
server is not native or deployed acceptance.
