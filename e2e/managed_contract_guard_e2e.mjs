// Negative / contract-guard conformance for Managed Agents, driven by the official
// Anthropic TypeScript SDK against awaken-server-local (echo model).
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
      // vault state is wired; that path is covered by managed_mcp_e2e.mjs and the
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
