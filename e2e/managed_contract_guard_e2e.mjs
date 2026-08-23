// Negative / contract-guard conformance for Managed Agents, driven by the official
// Anthropic TypeScript SDK against awaken-server (echo model).
//
// The SDK's happy path never sends a malformed request, so the error contract is
// easy to leave undertested. This locks the shapes an SDK client relies on when
// things go wrong: 404 + not_found_error for a missing session (retrieve AND
// send), 400 + invalid_request_error for a malformed body, 404 for a session that
// references an unknown vault (and nothing is provisioned), and the archive
// lifecycle (archived_at is set; the session becomes read-only).
//
// Run: (from e2e/)  node managed_contract_guard_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_BETA = 'agent-memory-2026-07-22';
const PORT = Number(process.env.E2E_PORT ?? 38142);

// Raw POST /v1/sessions with the beta + version headers the SDK would send, so the
// request reaches the handler (used where we need a shape the SDK typings reject).
async function rawCreate(baseUrl, body) {
  return fetch(`${baseUrl}/v1/sessions`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'anthropic-version': '2023-06-01',
      'anthropic-beta': BETAS.join(','),
      'x-api-key': 'e2e-dummy',
    },
    body,
  });
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Causes: ordinary Managed collection/write with correct, missing, or
      // memory-only beta header. Constraint: the dated Managed beta is required on
      // every verb, before domain handling. Effects: missing/wrong header returns
      // 400 and creates nothing; the SDK's correct header proceeds below. Rules A1/A2/A5.
      for (const [rule, path, method, beta] of [
        ['A2 collection missing', '/v1/sessions', 'GET', null],
        ['A2 create missing', '/v1/sessions', 'POST', null],
        ['A5 memory-only', '/v1/sessions', 'GET', 'agent-memory-2026-07-22'],
      ]) {
        const response = await fetch(`${baseUrl}${path}`, {
          method,
          headers: {
            'content-type': 'application/json',
            'x-api-key': 'e2e-dummy',
            ...(beta ? { 'anthropic-beta': beta } : {}),
          },
          ...(method === 'POST' ? { body: JSON.stringify({ agent: 'assistant' }) } : {}),
        });
        assert.equal(response.status, 400, rule);
        const error = await response.json();
        assert.equal(error.error.type, 'invalid_request_error', rule);
      }
      const afterHeaderRejects = [];
      for await (const session of client.beta.sessions.list({ betas: BETAS })) {
        afterHeaderRejects.push(session);
      }
      assert.equal(afterHeaderRejects.length, 0, 'A2 rejected create has no mutation');
      pass('Managed beta header decision rules A1/A2/A5 reject before mutation');

      // Causes: old SDK Managed-only, current SDK Memory-only, missing, unknown,
      // or both endpoint betas. Constraint: one recognized selector is required
      // and the two official selectors are never combined. Effects: either SDK
      // reaches the one current handler; ambiguous/absent selectors reject
      // before resource lookup or mutation.
      for (const [rule, beta, expected] of [
        ['A3 current memory-only', MEMORY_BETA, 200],
        ['A3 legacy managed-only', BETAS[0], 200],
        ['A4 both', `${MEMORY_BETA},${BETAS[0]}`, 400],
        ['A4 unknown-only', 'future-memory-beta', 400],
        ['A4 missing', null, 400],
      ]) {
        const response = await fetch(`${baseUrl}/v1/memory_stores`, {
          headers: {
            'x-api-key': 'e2e-dummy',
            ...(beta ? { 'anthropic-beta': beta } : {}),
          },
        });
        assert.equal(response.status, expected, rule);
        if (expected === 400) {
          assert.equal((await response.json()).error.type, 'invalid_request_error', rule);
        }
      }
      for (const [rule, beta] of [
        ['A4 both cannot create', `${MEMORY_BETA},${BETAS[0]}`],
        ['A4 unknown cannot create', 'future-memory-beta'],
        ['A4 missing cannot create', null],
      ]) {
        const response = await fetch(`${baseUrl}/v1/memory_stores`, {
          method: 'POST',
          headers: {
            'content-type': 'application/json',
            'x-api-key': 'e2e-dummy',
            ...(beta ? { 'anthropic-beta': beta } : {}),
          },
          body: JSON.stringify({ name: `must-not-exist-${rule}` }),
        });
        assert.equal(response.status, 400, rule);
      }
      const stores = await fetch(`${baseUrl}/v1/memory_stores`, {
        headers: { 'x-api-key': 'e2e-dummy', 'anthropic-beta': MEMORY_BETA },
      });
      assert.equal(stores.status, 200);
      assert.deepEqual((await stores.json()).data, [], 'header rejects perform no Memory mutation');
      pass('Memory beta rules accept both SDK generations and reject ambiguity before mutation');

      // --- retrieve unknown session -> 404 + not_found_error envelope ---
      await assert.rejects(
        () => client.beta.sessions.retrieve('sesn_does_not_exist', { betas: BETAS }),
        (err) => {
          assert.equal(err.status, 404);
          assert.equal(err.error?.type, 'error');
          assert.equal(err.error?.error?.type, 'not_found_error');
          assert.ok(err.error?.error?.message, 'error message is populated');
          return true;
        },
      );
      pass('retrieve unknown session -> 404 + not_found_error');

      // --- send to unknown session -> 404 (the write path guards too) ---
      // Negative rule N1: C1 unknown id => E1 SDK 404 and no receipt. Constraint
      // K1: without admission there is no processed-receipt observation path.
      await assert.rejects(
        () =>
          client.beta.sessions.events.send('sesn_does_not_exist', {
            events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
            betas: BETAS,
          }),
        (err) => {
          assert.equal(err.status, 404);
          assert.equal(err.error?.error?.type, 'not_found_error');
          return true;
        },
      );
      pass('send to unknown session -> 404 + not_found_error');

      // --- malformed body -> 400 + invalid_request_error envelope ---
      const malformed = await rawCreate(baseUrl, '{ not valid json');
      assert.equal(malformed.status, 400);
      const malformedBody = await malformed.json();
      assert.equal(malformedBody.type, 'error');
      assert.equal(malformedBody.error.type, 'invalid_request_error');
      assert.ok(malformedBody.error.message, 'decode-failure message is populated');
      pass('malformed body -> 400 + invalid_request_error');

      // (vault-not-found on create is a 404 only in the MCP-enabled build where
      // vault state is wired; that path is covered by managed_mcp_e2e.ts and the
      // Rust `unknown_vault_id_fails_the_create_with_404` test — not echo mode.)

      // --- archive lifecycle: archived_at is set; the session goes read-only ---
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const archived = await client.beta.sessions.archive(session.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archive stamps archived_at');
      const reread = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
      assert.ok(reread.archived_at, 'the archived timestamp survives a re-read');
      // Archive is terminal in this server: status -> terminated and a
      // session.status_terminated event is committed to the log.
      assert.equal(reread.status, 'terminated', 'archive moves the session to terminated');
      const evs = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) evs.push(ev);
      assert.ok(
        evs.some((e) => e.type === 'session.status_terminated'),
        'archive commits a session.status_terminated event',
      );
      pass('archive -> archived_at + status terminated + session.status_terminated event');

      // An archived session is read-only: events.send is refused with 409.
      // Negative rule N2: C2 archived terminal => E2 SDK 409 and no receipt;
      // K2 forbids treating the independently listed archive event as send
      // completion. Decisions N1=C1=>E1; N2=C2=>E2.
      await assert.rejects(
        () =>
          client.beta.sessions.events.send(session.id, {
            events: [{ type: 'user.message', content: [{ type: 'text', text: 'after archive' }] }],
            betas: BETAS,
          }),
        (err) => {
          assert.equal(err.status, 409, `archived write -> 409 (got ${err.status})`);
          assert.equal(err.error?.error?.type, 'invalid_request_error');
          return true;
        },
      );
      pass('archived session is read-only: events.send -> 409 invalid_request_error');
    });

    console.log('E2E PASS: Managed Agents negative/contract-guard shapes via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
// SDK-root cause/effect graph: an official Beta declaration family is present
// and therefore must have exactly one behavior owner; an unmapped family makes
// the compatibility claim fail. Decision table: known mapped family -> continue;
// unknown or duplicate family -> fail before any compatibility result is printed.
