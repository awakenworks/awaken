// Orthogonal Environment × SandboxExecutionPolicy × Resource × protocol E2E.
// Every row creates the authoritative Managed Session first, then executes that
// exact thread through a non-Managed adapter. No adapter receives configuration.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { FILES_BETA, withScenarioServer, pass, streamedText } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', FILES_BETA];

async function runProtocol(base, protocol, thread, marker) {
  if (protocol === 'ai-sdk') {
    const response = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        messages: [{ id: `u-${marker}`, role: 'user', parts: [{ type: 'text', text: marker }] }],
      }),
    });
    const text = await response.text();
    return { status: response.status, wire: streamedText(text) || text };
  }
  if (protocol === 'ag-ui') {
    const response = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread, runId: `run-${marker}`,
        messages: [{ id: `u-${marker}`, role: 'user', content: marker }],
        tools: [], context: [], state: {}, forwardedProps: {},
      }),
    });
    const text = await response.text();
    return { status: response.status, wire: streamedText(text) || text };
  }
  const response = await fetch(`${base}/v1/a2a`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0', id: marker, method: 'message/send',
      params: { message: {
        messageId: `u-${marker}`, contextId: thread, role: 'user', kind: 'message',
        parts: [{ kind: 'text', text: marker }],
      } },
    }),
  });
  return { status: response.status, wire: await response.text() };
}

async function main() {
  await withScenarioServer('environment-matrix', 'echo', 38731, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('orthogonal resource bytes'), 'matrix.txt'), betas: BETAS,
    });
    const environments = {};
    for (const network of ['unrestricted', 'none']) {
      const env = await client.beta.environments.create({
        name: `matrix-${network}`,
        config: network === 'unrestricted'
          ? { type: 'cloud', networking: { type: 'unrestricted' } }
          : { type: 'cloud', networking: { type: 'limited' } },
        betas: BETAS,
      });
      environments[network] = env.id;
    }
    const policyId = `matrix-workdir-${process.pid}`;
    let response = await fetch(`${base}/v1/awaken/sandbox-execution-policies`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id: policyId, config: { isolation: 'workdir' } }),
    });
    assert.equal(response.status, 201, await response.text());
    for (const environmentId of Object.values(environments)) {
      response = await fetch(`${base}/v1/awaken/environments/${environmentId}/sandbox-execution-policy`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ policy_id: policyId, version: 1 }),
      });
      assert.equal(response.status, 200, await response.text());
    }

    let rows = 0;
    for (const network of ['unrestricted', 'none']) {
      for (const sandbox of ['default', 'workdir']) {
        for (const resource of ['absent', 'file']) {
          for (const protocol of ['ai-sdk', 'ag-ui', 'a2a']) {
            // The default axis uses an unbound duplicate Environment; the workdir
            // axis uses the exact-bound one. This avoids mutating a binding per row.
            let environmentId = environments[network];
            if (sandbox === 'default') {
              const env = await client.beta.environments.create({
                name: `matrix-default-${network}-${rows}`,
                config: network === 'unrestricted'
                  ? { type: 'cloud', networking: { type: 'unrestricted' } }
                  : { type: 'cloud', networking: { type: 'limited' } },
                betas: BETAS,
              });
              environmentId = env.id;
            }
            const resources = resource === 'file'
              ? [{ type: 'file', file_id: file.id, mount_path: '/workspace/matrix.txt' }]
              : [];
            const session = await client.beta.sessions.create({
              agent: 'assistant', environment_id: environmentId, resources, betas: BETAS,
            });
            const marker = `MATRIX-${rows}-${network}-${sandbox}-${resource}-${protocol}`;
            const result = await runProtocol(base, protocol, session.id, marker);
            // Workdir materialization cause/effect table. The Echo executor is
            // in-process, so without a Resource there is no sandbox workload on
            // which to enforce networking. A File forces materialization and its
            // read-only guarantee is the first `prepare_environment` capability
            // gate (masking any later network check).
            // | File RO mount | sandbox workload exists | effect |
            // | false | false | success; frozen network remains unconsumed |
            // | true  | true  | fail: read-only unsupported |
            const expectedFailure = resource === 'file'
              ? /does not enforce read-only/
              : null;
            if (expectedFailure) {
              // AG-UI legitimately echoes the caller-provided run id in RUN_STARTED;
              // the typed terminal error, not marker absence, proves no model turn.
              assert.match(result.wire, expectedFailure, `${marker}: ${result.wire.slice(0, 500)}`);
            } else {
              assert.equal(result.status, 200, `${marker}: ${result.wire.slice(0, 300)}`);
              assert.ok(result.wire.includes(marker), `${marker}: ${result.wire.slice(0, 300)}`);
            }
            const projected = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
            assert.equal(projected.environment_id, environmentId);
            assert.equal(projected.resources.length, resources.length);
            rows += 1;
          }
        }
      }
    }
    assert.equal(rows, 24);
    pass('24-row Environment × sandbox × Resource × protocol success/fail-closed decision table');
  });
  console.log('E2E PASS: all protocol adapters consume the same frozen Environment/Resource baseline.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
