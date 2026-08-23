// Offline completeness gate for the Managed Agents documentation traceability
// inventory. It owns no behavior or test-design mapping: executable tests own
// their adjacent cause/effect rules, while this gate pins the official sitemap
// snapshot and rejects missing, duplicate, or unevidenced rows.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const E2E = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const report = readFileSync(resolve(E2E, 'MANAGED_AGENTS_DOCS_COVERAGE.md'), 'utf8');

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
  'mcp-tunnels',
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

console.log(`MANAGED DOCS COVERAGE PASS: ${rows.length}/${officialPages.length} official pages mapped once.`);
