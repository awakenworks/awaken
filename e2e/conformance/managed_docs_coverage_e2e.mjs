// Offline completeness gate for the Managed Agents documentation traceability
// inventory. It owns no behavior or test-design mapping: executable tests own
// their adjacent cause/effect rules, while this gate pins the official sitemap
// snapshot and rejects missing, duplicate, or unevidenced rows.

import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { MANAGED_TS_METHOD_MANIFEST } from './managed_ts_sdk_method_manifest.mjs';

const E2E = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const reportPath = resolve(E2E, 'MANAGED_AGENTS_DOCS_COVERAGE.md');
let report = readFileSync(reportPath, 'utf8');

const officialPages = [
  'agent-setup',
  'budgets',
  'cloud-sandboxes-reference',
  'define-outcomes',
  'dreams',
  'environments',
  'events-and-streaming',
  'files',
  'github',
  'mcp-connector',
  'memory',
  'migration',
  'multiagent-orchestration',
  'onboarding',
  'overview',
  'permission-policies',
  'quickstart',
  'reference',
  'scheduled-deployments',
  'self-hosted-sandboxes',
  'self-hosted-sandboxes-security',
  'session-operations',
  'sessions',
  'skills',
  'tools',
  'vaults',
  'webhooks',
];

// Causes: official sitemap membership and one canonical report row per slug.
// Constraints: each row has contract, executable evidence, boundary, and explicit
// local/external classification; this gate must stay offline and deterministic.
// Effects: missing/duplicate/extra/empty rows fail CI before coverage can be claimed.
// Decision rule: DOC1 exact set, DOC2 unique, DOC3 four populated cells, DOC4 status.
const rows = [...report.matchAll(
  /^\| \[([^\]]+)\]\(https:\/\/platform\.claude\.com\/docs\/en\/managed-agents\/([^\)]+)\) \|([^\n]+)$/gm,
)];
assert.equal(rows.length, officialPages.length, 'DOC1: exact number of official page rows');
const slugs = rows.map((match) => match[2]);
assert.deepEqual([...slugs].sort(), [...officialPages].sort(), 'DOC1: exact official sitemap snapshot');
assert.equal(new Set(slugs).size, slugs.length, 'DOC2: every page occurs exactly once');

for (const match of rows) {
  const [label, slug, remainder] = [match[1], match[2], match[3]];
  assert.equal(label, slug, `DOC3: ${slug} link label matches slug`);
  const cells = remainder.split('|').map((cell) => cell.trim()).filter(Boolean);
  assert.equal(cells.length, 3, `DOC3: ${slug} has contract, evidence, and boundary cells`);
  assert.ok(cells.every((cell) => cell.length >= 12), `DOC3: ${slug} cells are substantive`);
  assert.match(cells[1], /`[^`]+\.(?:mjs|ts|rs)`/, `DOC3: ${slug} names executable evidence`);
  assert.match(cells[2], /(?:✅|◇)/, `DOC4: ${slug} states local/external scope`);
}

const methodSectionStart = '<!-- managed-sdk-method-coverage:start -->';
const methodSectionEnd = '<!-- managed-sdk-method-coverage:end -->';
const methodEvidence = (entry) => entry.sdkHelper
  ? `\`${entry.owner}\` via \`${entry.sdkHelper}\``
  : `\`${entry.owner}\``;
const expectedMethodSection = [
  methodSectionStart,
  '| Official TypeScript SDK method | Route family | Coverage | Executable E2E scenario |',
  '|---|---|---|---|',
  ...MANAGED_TS_METHOD_MANIFEST.map((entry) =>
    `| \`${entry.sdkMethod}\` | \`${entry.route}\` | ✅ covered | ${methodEvidence(entry)} |`),
  methodSectionEnd,
].join('\n');
let methodStart = report.indexOf(methodSectionStart);
let methodEnd = report.indexOf(methodSectionEnd);

if (process.env.AWAKEN_UPDATE_MANAGED_DOCS === '1') {
  assert.ok(methodStart >= 0 && methodEnd > methodStart, 'DOC5: method coverage markers exist once');
  report = report.slice(0, methodStart)
    + expectedMethodSection
    + report.slice(methodEnd + methodSectionEnd.length);
  writeFileSync(reportPath, report, 'utf8');
  methodStart = report.indexOf(methodSectionStart);
  methodEnd = report.indexOf(methodSectionEnd);
}

// Method-report cause/effect graph: C1=the installed SDK method set maps exactly
// once to MANAGED_TS_METHOD_MANIFEST (owned by the adjacent manifest test);
// C2=every manifest entry names one route family and executable E2E owner;
// C3=this report materializes that exact ordered manifest. Effects: E1=every SDK
// method has an explicit covered status and scenario; E2=missing, duplicate,
// hand-edited, or stale rows fail the offline docs gate. Constraint K1: the
// manifest remains the sole method inventory; this Markdown is a checked
// projection and owns no API or test truth. Decision rule DOC5: C1+C2+C3->E1;
// !C3->E2, while C1/C2 failures remain owned by the manifest test.
assert.ok(methodStart >= 0 && methodEnd > methodStart, 'DOC5: method coverage markers exist once');
assert.equal(report.lastIndexOf(methodSectionStart), methodStart, 'DOC5: start marker is unique');
assert.equal(report.lastIndexOf(methodSectionEnd), methodEnd, 'DOC5: end marker is unique');
assert.equal(
  report.slice(methodStart, methodEnd + methodSectionEnd.length),
  expectedMethodSection,
  'DOC5: method coverage appendix is the exact canonical manifest projection',
);

if (process.env.AWAKEN_MANAGED_DOCS_LIVE === '1') {
  // Live-inventory cause/effect graph: C4=the explicit online audit is enabled;
  // C5=the official Claude docs index is reachable and contains its current
  // Managed Agents page links. Effects: E3=the live unique page set equals the
  // offline reviewed snapshot; E4=network failure or page drift fails loudly.
  // Constraint K2: live verification is optional and reads the same
  // `officialPages` authority; deterministic CI remains offline and no second
  // page inventory is persisted. Decision rule DOC6: C4+C5->E3; C4+!C5->E4.
  const response = await fetch('https://platform.claude.com/docs/llms.txt');
  assert.ok(response.ok, `DOC6: official docs index HTTP ${response.status}`);
  const livePages = [...new Set(
    [...(await response.text()).matchAll(
      /https:\/\/platform\.claude\.com\/docs\/en\/managed-agents\/([a-z0-9-]+)/g,
    )].map((match) => match[1]),
  )].sort();
  assert.deepEqual(livePages, [...officialPages].sort(), 'DOC6: live official page set matches snapshot');
}

console.log(
  `MANAGED DOCS COVERAGE PASS: ${rows.length}/${officialPages.length} official pages and `
  + `${MANAGED_TS_METHOD_MANIFEST.length}/${MANAGED_TS_METHOD_MANIFEST.length} SDK methods mapped once.`,
);
