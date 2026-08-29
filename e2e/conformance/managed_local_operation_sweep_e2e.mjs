// Exercise the generated Managed operation ledger against a real local Awaken
// process. The hosted/release gate reuses the same sweep against public ingress;
// this local owner makes malformed-body and missing-resource error boundaries a
// mandatory checkout gate rather than evidence available only with credentials.

import assert from 'node:assert/strict';
import fs from 'node:fs';

import { exerciseDeployedOperationSweep } from '../../packages/managed-sdk-oracle/src/conformance/deployed-sweep.mjs';
import { pass, withRealServer } from '../harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38196);
const coverage = JSON.parse(fs.readFileSync(
  new URL('../../contracts/anthropic-managed/operation-coverage.generated.json', import.meta.url),
  'utf8',
));

// Test design: every_generated_operation_has_a_real_process_error_boundary
//
// Cause/effect graph:
// generated SDK + documented operation ledger
//   -> one concrete missing-resource or malformed-create request per operation
//   -> production router/extractor/application boundary in a real process
//   -> either a typed collection page or an Anthropic JSON error envelope.
//
// This detects a route absent from composition, a raw Axum Query/Json/Multipart
// rejection, an HTML/plain-text proxy response, a 5xx caused by negative input,
// and an accidentally successful mutation. The generated ledger and shared
// deployed sweep are the only inventories, so adding an SDK operation makes this
// test fail closed without another hand-maintained list.
//
// Decision table:
// | operation partition        | request witness             | required effect |
// | collection GET, no parent  | empty collection query      | 200 + data[]    |
// | create/update/action       | malformed JSON or wrong media| canonical 4xx   |
// | item/nested collection     | generated absent identifiers| canonical 4xx   |
// | any partition              | route/proxy/internal failure | test failure     |
await withRealServer('echo', PORT, async (baseURL) => {
  const results = await exerciseDeployedOperationSweep({
    actual: {
      name: 'Awaken local real process',
      baseURL,
      apiKey: 'e2e-dummy', // awaken-allow: secret
      tunnelAccessToken: 'e2e-dummy-tunnel', // awaken-allow: secret
    },
    coverage,
  });

  assert.equal(results.length, coverage.operations.length, 'every operation was exercised');
  assert.deepEqual(
    results.map(({ id }) => id),
    coverage.operations.map(({ id }) => id),
    'the sweep preserves the complete generated operation identity set',
  );
  pass(`${results.length} Managed operations expose a real-process canonical boundary`);
}, { mode: 'management' });
