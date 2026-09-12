// Independent business oracles. Never accept a model's self-reported "passed"
// or derive the expected arithmetic/dates/receipts from its generated answer.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import Ajv from 'ajv';

const ajv = new Ajv({ allErrors: true });
const invoiceSchema = ajv.compile({
  type: 'object', required: ['source_version', 'invoice_id', 'issued_at', 'currency', 'subtotal', 'purchase_order', 'line_items'],
  additionalProperties: false,
  properties: {
    source_version: { type: 'string' }, invoice_id: { const: 'INV-204' },
    issued_at: { const: '2026-09-03' }, currency: { const: 'SGD' }, subtotal: { type: 'number' },
    purchase_order: { type: 'null' },
    line_items: { type: 'array', minItems: 2, maxItems: 2, items: { type: 'object', required: ['quantity', 'unit_price'], additionalProperties: false, properties: { quantity: { type: 'number' }, unit_price: { type: 'number' } } } },
  },
});
const setEquals = (actual, expected, label = 'exact values') => {
  assert.ok(Array.isArray(actual), `${label}: expected an array`);
  assert.deepEqual([...actual].sort(), [...expected].sort(), label);
};

export function assertTemplateResult(scenario, state, artifacts) {
  for (const name of scenario.artifacts) assert.ok(artifacts[name]?.length, `${name} must be harvested and downloaded`);
  const result = JSON.parse(artifacts['result.json'].toString());
  assert.equal(result.source_version, scenario.version, 'output must carry the version obtained from actual source data');
  const effects = (connector) => state.effects.filter((effect) => effect.connector === connector);
  const one = (connector) => { const rows = effects(connector); assert.equal(rows.length, 1, `one ${connector} effect`); return rows[0]; };
  const called = (connector, name) => assert.ok(state.calls.some((call) => call.connector === connector && call.name === name), `${connector}.${name} actually executed`);
  switch (scenario.id) {
    case 'deep-research': {
      setEquals(result.latencies_ms, [180, 260], 'both contradictory measurements');
      assert.equal(result.production_evidence, false);
      assert.equal(result.recommendation, 'pilot');
      setEquals(result.source_urls, scenario.records.research.map((r) => r.url), 'exact citations');
      const text = artifacts['brief.md'].toString();
      for (const url of result.source_urls) assert.ok(text.includes(url), 'brief contains each source link');
      assert.match(text, /180/); assert.match(text, /260/);
      for (const id of ['study-a', 'study-b']) assert.ok(state.calls.some((c) => c.name === 'read_source' && c.input.id === id), 'each source was read');
      break;
    }
    case 'structured-extractor':
      assert.ok(invoiceSchema(result), JSON.stringify(invoiceSchema.errors));
      assert.equal(result.subtotal, 53.5, '3*12.5 + 2*8');
      assert.equal(result.line_items.reduce((sum, x) => sum + x.quantity * x.unit_price, 0), 53.5);
      break;
    case 'field-monitor': {
      setEquals(result.included_ids, ['release-new', 'benchmark-new'], 'date-window inclusion');
      setEquals(result.excluded_ids, ['release-old'], 'old material excluded');
      const page = one('notion'); assert.equal(result.page_id, page.id);
      for (const id of result.included_ids) assert.ok(page.body.includes(`https://sources.fixture.invalid/${id}`));
      assert.equal(page.external_id, 'orchid-2026-09-13');
      break;
    }
    case 'support-agent': {
      called('notion', 'read_page'); assert.equal(result.answerable, false);
      const message = one('slack'); assert.equal(result.escalation_id, message.id);
      assert.match(message.body, /SAML/i); assert.ok(message.body.includes(scenario.records.notion[0].url));
      assert.equal(message.external_id, 'saml-handoff');
      break;
    }
    case 'incident-commander': {
      called('sentry', 'get_issue'); called('github', 'read_file');
      assert.equal(result.release, 'r42'); assert.equal(result.affected_users, 37);
      assert.match(result.hypothesis, /None|null|guard|absent/i);
      const ticket = one('linear'), message = one('slack');
      assert.equal(result.ticket_id, ticket.id); assert.equal(result.status_id, message.id);
      assert.equal(ticket.source_id, 'INC-17'); assert.ok(message.body.includes(ticket.url));
      assert.equal(result.next_update_minutes, 15);
      break;
    }
    case 'contract-clause-extraction': {
      called('box', 'read_file'); assert.equal(result.payment_due, '2026-10-15');
      assert.equal(result.renewal_notice_due, '2026-12-01', '30 calendar days before Dec 31');
      assert.match(JSON.stringify(result.ambiguous_obligations), /training/i);
      const tasks = effects('asana'); assert.equal(tasks.length, 2);
      setEquals(result.task_ids, tasks.map((t) => t.id), 'returned task receipts');
      setEquals(tasks.map((t) => t.due_date), ['2026-10-15', '2026-12-01'], 'actual task due dates');
      for (const task of tasks) {
        assert.equal(task.source_id, 'contract-17');
        assert.ok(task.body.includes(task.due_date === '2026-10-15'
          ? 'Payment of SGD 1200 is due on 2026-10-15.'
          : 'Renewal notice must be received 30 calendar days before expiry on 2026-12-31.'), 'task quotes the full controlling clause');
      }
      break;
    }
    case 'sprint-retro-facilitator': {
      called('linear', 'list_issues'); called('slack', 'read_channel');
      setEquals(result.completed_ids, ['LIN-1', 'LIN-2']); setEquals(result.slipped_ids, ['LIN-3']);
      assert.equal(result.average_completed_cycle_days, 3);
      const text = execFileSync('python3', ['-c', 'import sys,io,zipfile,xml.etree.ElementTree as E; z=zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())); assert "[Content_Types].xml" in z.namelist(); root=E.fromstring(z.read("word/document.xml")); print(" ".join(root.itertext()))'], { input: artifacts['retro.docx'], encoding: 'utf8' });
      for (const phrase of ['Went well', 'Dragged', 'Try next sprint', 'LIN-3', 'MSG-1']) assert.ok(text.includes(phrase), `DOCX content ${phrase}`);
      break;
    }
    case 'support-to-eng-escalator': {
      called('intercom', 'get_conversation');
      assert.equal(result.reproduced, true); assert.equal(result.error_type, 'TypeError');
      assert.match(artifacts['repro.log'].toString(), /TypeError/);
      const ticket = one('atlassian'), note = one('intercom'), message = one('slack');
      assert.equal(ticket.source_id, 'CONV-7'); assert.match(ticket.body, /TypeError/);
      assert.equal(result.ticket_id, ticket.id); assert.equal(result.note_id, note.id); assert.equal(result.status_id, message.id);
      assert.ok(note.body.includes(ticket.url)); assert.ok(message.body.includes(ticket.url));
      break;
    }
    case 'data-analyst': {
      called('amplitude', 'query');
      assert.equal(result.valid_orders, 2); assert.equal(result.duplicates_removed, 1); assert.equal(result.missing_rows_excluded, 1);
      assert.deepEqual(result.revenue_by_region, { North: 100, South: 90 }); assert.equal(result.total_revenue, 190);
      assert.equal(result.conversion_rate, 0.2);
      const csv = artifacts['totals.csv'].toString(); assert.match(csv, /North[^\n]*100/); assert.match(csv, /South[^\n]*90/);
      const svg = artifacts['chart.svg'].toString(); assert.match(svg, /North/); assert.match(svg, /South/);
      execFileSync('python3', ['-c', 'import sys,xml.etree.ElementTree as E; r=E.fromstring(sys.stdin.buffer.read()); assert r.tag.endswith("svg"); assert len(list(r.iter())) > 3'], { input: artifacts['chart.svg'] });
      break;
    }
    default: throw new Error('No business oracle for template');
  }
  return result;
}
