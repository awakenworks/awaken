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
- Canonical runtime full-DTO readers may separately be a native verified direct
  Service with the exact current definition-read and independent runtime action.
  A Service's run permission or Workspace wildcard alone grants no definition
  content, personal preference or human definition-management action. Delegated
  actors and unverified credentials cannot enter this runtime read lane.
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
only new selection also requires current use and an executable publication. The shared native tenancy kernel reconciles explicit selectors; an installed
default does not override an addressed Workspace. This surface still requires
trusted routing context, and a Header alone is not a fallback company. Invalid,
conflicting or mismatched execution selectors are refused before fine PDP.

The deployment file may explicitly declare `agent_governance.product_id`. The
native product's exact Agent vocabulary and the Workspace PAP candidate are
separate declarations: wildcard legacy profile patterns are not product
vocabulary. The declaration helpers perform no registration, activation,
projection or grant writes. Missing configuration is unavailable. Native IAM
remains responsible for validating the product namespace and immutable profile
owner before activation; a supplied product id is never evidence of permission.

The declaration also owns adoption of the additional fine-definition middleware.
Without it, the new directory and personal-selection endpoints remain closed;
the existing Config and Managed Agents endpoints retain their original current
IAM Workspace/runtime authorization. An unadopted optional surface must not
make those existing endpoints require a new product or a human identity.
This reuses the original guards and stores and grants no new personal authority.
With an explicit declaration, the registered-resource definition guards apply:
missing rights, an invalid product, stale proof or an unavailable dependency
remain refusals. No request, failed decision or unknown resource can fall back
from an adopted definition contract to Workspace authority. Deployment adoption
is distinct from either authentication or permission.
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
fields in GET/history/events/SSE. API-date and history-time selectors use the original native typed presentation
owner. Private manifests and optional event omission compose with that owner,
not a second DTO serializer or stream parser. Known structural projections preserve message
content, tool outputs and citations; arbitrary nested keys are not recursively
scrubbed. Definition audit permission grants no personal conversation read.

Pure tests, real adapters, native IAM/HTTP, full repository hooks and deployed
browser journeys remain distinct claims. A skipped fixture or unsupported old
server is not native or deployed acceptance.

## Existing definition and conversation ingress

The enterprise edge protects the original Managed and Config owners rather than
copying definitions into a directory store. Local open mode and explicitly
provisioned fixed-operator tools retain their separate contracts. An execution
Workspace or a hosted model tool call does not prove a current human management
request.

| Case | Entry or cause | Required effect |
| --- | --- | --- |
| D1 | Workspace read, use, or personal selection without definition read | Managed and Config detail/version and publication/export deny; collections filter by current fine permission before their original pagination |
| D2 | Native product/proof/freshness missing, fault, policy drift or Config ABA | Abort the entire definition page; no partial data, cached Allow or hidden-item skip |
| D3 | Exact current definition read | Original full DTO and immutable versions remain readable for that Agent; other Agent definitions and hidden page boundaries remain unavailable |
| D4 | Config draft PUT | Current direct Account and exact definition write; explicit caller generation required before audit; preserve original Config CAS and minimal native receipt |
| D5 | Managed create/update/archive, direct publish, unbound preview, assistant ensure or execution-evidence collection | Enterprise legacy ingress refuses before audit or Config effects until the missing reviewed lifecycle contract exists |
| D6 | Config resources read or write | Read requires exact definition read; the legacy resources write remains closed without a reviewed native baseline contract |
| D7 | Hosted CurrentAttempt draft/validate/catalog tools or MCP export | No verified human management admission means explicit tool refusal before inventory, definition, audit or resource effects; a fixed local operator remains independently usable |
| D8 | Private Session/Thread DTO, list, history and SSE | Original ordinary-Agent identity manifest (including declared execution identity for the selected current API date) only in known Agent slots; preserve the Advisor variant’s original type/model identity; no system, model, tools, MCP, skills or child definitions; preserve original id, revision/ETag, cursor, timestamps, content, metadata and citations |
| D9 | Canonical complete Session/Thread DTO and mutation response | Preserve official required fields behind independent current definition read for the root and frozen child Agent definitions; direct Account and separately native verified Service retain independent run/owner admission; use does not imply definition read and audit does not imply conversation read |
| D10 | Canonical history and SSE in enterprise Cloud admission | Omit the optional session.updated Agent snapshot consistently in the native serializer; preserve original committed events, pending replies and processing enrichment |

Existing service reconciliation may finish the effects of an already admitted
durable management command. It does not authenticate a fresh model tool call or
create a new confirmation, definition or audit intent.

The legacy definition matrix uses actual IAM HTTP with native SQLite and required
PostgreSQL Config/IAM owners. It exercises the actual Config and Managed Agent
routers, including original audit and CAS ports. The pre-fix negative control is
a hidden stored definition returned by the old Workspace-only read guard.
Only the newly governed canonical DTO requests select single-attempt fenced
runtime evaluation in the original Cloud guard. Its verified principal, trusted
Workspace and opaque fence must match the original fine admission; this marker
is private in-process state, not an HTTP input or a grant. Existing legacy
transport defaults remain separate. A failed current run decision aborts before
the Session reader, even when a later attempt would succeed.

The Session admission edge additionally uses an explicit downstream DTO/commit
counter fixture with actual native IAM to distinguish run permission, root/child
definition permission and post-attempt identity loss. Its direct Service matrix
requires explicit exact grants, rejects a missing/withdrawn child grant, aborts
on a single failed runtime decision, fences a foreign Workspace and retains the
same original complete runtime snapshot only when both permissions qualify.
The current strict read lane accepts the native verified direct Service JWT
family. Native API-key introspection currently supplies no verified issuer in
its identity receipt, so that family cannot be asserted as qualified for the
new issuer-bound read contract. It fails closed until its native owner supplies
that coordinate; the host must not guess a deployed token family from a filename.
The Service is still refused by human definition writes, owner collections and
private assistant entry. Exact Config/Managed detail, version and immutable
publication/export reads may independently qualify the same explicit Service
read permission; no read authorizes shared-definition management. Existing backend profiles without these new fine grants remain a real
adoption gap; this boundary never repairs them by granting authority. Its counter is not a
claim of a native Session SQL commit or exactly-once execution.
Private wire tests use the original Managed application fixture, its repository,
warm/cold request views and original native event history/SSE. Tool-update cases
explicitly enable the existing runtime fixture's disposable replacement recorder
and check the exact client-tool projection; its default unsupported behavior is
retained for other fixtures. These qualify the
protocol projection and ownership boundary, not a browser journey. Pure
presentation tests additionally require byte-identical legacy full serialization
and identical private history/SSE payloads for each committed id.

Canonical full Session DTO inspection has a finite 2 MiB response/body admission
capacity and at most 4096 distinct frozen Agent coordinates. Exceeding it fails
closed rather than returning a partial definition. This capacity does not change
the native private Session or event/stream content limits: their known Agent
slots are projected before serialization without another stream parser. A failed
ending guard after a canonical persistent attempt is unknown 503; retain its
native original intent/key instead of treating it as an uncommitted retry.

This boundary is not Studio review provenance. It creates no reviewer identity,
author record, synthetic published state, default role or automatic product
registration. Complete draft creation, responsible review by someone other than
the author, preview qualification and governed publication require their native
lifecycle contract before those UI actions can be enabled. This test design also
does not claim that directory selection alone qualifies engine execution or any
Desk Market/MyAgents journey.

## Current-owner selection list qualification

`GET /v1/me/agent-selections` observes persisted preferences for the current
verified issuer and Account in the trusted Workspace. It accepts only `limit`
(1 through 100, default 25), an opaque `after` cursor, and optional boolean
`selected`. Its whitelist is `data: [{agent_id, generation, selected}]`,
`next_page`, and `visibility=current-owner-authorized-selection-subset`.
It returns no name, definition, publication, executable version, permission flag,
or global total. An empty authorized subset does not prove global absence.

Each row independently requires current `agent.selection.read`; Agent use,
definition read and selection write confer none of that authority. Selection
read also confers no write or use. Filtering precedes pagination. A stale or
unreadable cursor is invalid rather than a way to discover hidden identifiers.
Each request reobserves the current snapshot and authority; multiple page reads
are not a frozen cross-request snapshot. Consumers refresh after a cursor becomes
invalid. Dependency failure rejects the whole page with 503.

The existing Config snapshot and its transactional scope epoch remain the read
owner. Its bounded preference query includes records whose definition no longer
exists; no second preference table or frontend selection truth is introduced.
Both definition heads and owner preferences retain independent limits of 4096;
overflow is unavailable, never silent truncation. Preference triggers already
advance the same epoch, including same-value updates and delete/reinsert.
The endpoint checks that epoch and current identity/authorization fence again
before returning. Corrupt identifiers or persisted zero generations are
unavailable rather than repaired into invented state.

A current generation can prepare an independently authorized deselection using
the original command protocol. It cannot resolve an earlier unknown write:
only the original-key outcome read observes that immutable receipt. A receipt's
applied generation may be older than the current preference generation. Neither
observation may substitute for the other or authorize a replacement key.

| Case | Setup and cause | Required effect | Executable owner |
| --- | --- | --- | --- |
| L1 | Select, revoke actual native use, retain independent read, reuse same JWT | Market hides manifest; owner list returns current preference without hidden definition | `selection_list_tests` SQLite/PostgreSQL HTTP matrices |
| L2 | Persisted preference without a current definition/publication | Minimal preference remains discoverable and independently removable; read exposes no definition | Both HTTP matrices and Config adapter orphan snapshot cases |
| L3 | Same ids under other Account, issuer or Workspace | Only current exact-owner rows, no caller-selected owner or fallback Workspace | Both HTTP matrices and shared Config orphan snapshot conformance |
| L4 | Hidden preference between two readable ids; limit one; unreadable/stale cursor | Filter before pagination, no hidden id/name/total or extra empty page; invalid cursor is 400 | Both HTTP matrices |
| L5 | Read revoked while write remains; write absent while read remains | Current list omits unreadable rows without claiming global absence; receipt GET denied; ungranted PUT denied | Both HTTP matrices |
| L6 | Deselect generation one; read current state and old original receipt | Current false generation two, immutable original true generation one; GET never changes epoch | Both HTTP matrices and orphan snapshot cases |
| L7 | Service/delegated/bad credential, missing trusted context or product declaration, malformed query | Current Account or explicit context required; finite 401/403/503/400, no new state | Both HTTP matrices |
| L8 | Strict PDP failure, failed observation, missing registration/freshness, mid-page policy drift | Whole request 503, no partial success or default false | Both HTTP matrices using actual Native IAM routes |
| L9 | Config changes and restores the same definition during row admission | Transactional epoch detects ABA and discards the captured page | Both HTTP matrices and existing adapter epoch conformance |
| L10 | Orphan preferences exceed requested native snapshot bound | Explicit store error, no truncated successful snapshot; owner and original receipt remain exact | SQLite/PostgreSQL Config adapter orphan snapshot cases |

These are required executable cases, not a claim of completed qualification.
Portable compilation, real adapters, actual Native IAM HTTP, prepared prefixed
router composition, browser journeys and deployed ingress remain separate test
layers. Unavailable PostgreSQL or a skipped fixture cannot qualify that layer.
