// Managed session sandbox-dispose lifecycle e2e, driven end-to-end by the official
// Anthropic TypeScript SDK against a real awaken-server — ONLY through the external
// managed-agents HTTP interface. Covers the terminal edges that reap a session's
// sandbox (ADR-0056): DELETE disposes + removes the session (later reads 404), and
// ARCHIVE disposes + tombstones it (status `terminated`, idempotent re-archive).
//
// The dir-reaping itself is asserted by the Rust unit/integration tests
// (host::tests::end_session_disposes_the_threads_sandbox and the managed state
// delete/archive tests); this e2e proves the SAME edges drive cleanly through the
// real server binary over the wire — a first turn provisions the sandbox, the
// terminal edge tears it down, and the server stays healthy across many cycles
// (a leak-per-session would surface here as accumulation/failure).
//
// Run: (from e2e/)  node managed_session_dispose_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38207);

async function drain(pageIter) {
  const out = [];
  for await (const item of pageIter) out.push(item);
  return out;
}

async function send(client, sid, text) {
  await client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

const agentTexts = (events) =>
  events
    .filter((e) => e.type === 'agent.message')
    .flatMap((e) => (e.content ?? []).map((c) => c.text ?? ''));

async function main() {
  try {
    await withServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // ── DELETE reaps the sandbox and removes the session ──────────────────
      const s1 = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(s1.status, 'idle', 'a fresh session is idle');

      // First turn lazily provisions the session's sandbox (ctx_for -> create_sandbox).
      await send(client, s1.id, 'hello');
      const e1 = await drain(client.beta.sessions.events.list(s1.id, { betas: BETAS }));
      assert.ok(
        agentTexts(e1).some((t) => t.startsWith('Echo:')),
        `the provisioned session ran a turn, got ${JSON.stringify(agentTexts(e1))}`,
      );
      pass('delete-path: session created + first turn provisioned the sandbox');

      // The terminal DELETE edge disposes the sandbox and drops the session.
      const del = await client.beta.sessions.delete(s1.id, { betas: BETAS });
      assert.equal(del.type, 'session_deleted', 'delete returns the terminal marker');

      // The session is gone: retrieve and events.list are now 404 (delete removes,
      // it does not tombstone). Reaching this cleanly proves the server drove the
      // end_session disposal without erroring on the deleted thread.
      await assert.rejects(
        () => client.beta.sessions.retrieve(s1.id, { betas: BETAS }),
        (err) => err?.status === 404,
        'a deleted session is no longer retrievable (404)',
      );
      pass('delete-path: DELETE reaped the sandbox and the session is 404 afterwards');

      // ── ARCHIVE reaps the sandbox and tombstones the session ──────────────
      const s2 = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await send(client, s2.id, 'hi again');
      const e2 = await drain(client.beta.sessions.events.list(s2.id, { betas: BETAS }));
      assert.ok(agentTexts(e2).some((t) => t.startsWith('Echo:')), 'archive-path session ran a turn');

      const archived = await client.beta.sessions.archive(s2.id, { betas: BETAS });
      assert.equal(archived.status, 'terminated', 'archive moves the session to terminated');

      // Archive tombstones (unlike delete): the session still reads back, terminal.
      const afterArchive = await client.beta.sessions.retrieve(s2.id, { betas: BETAS });
      assert.equal(afterArchive.status, 'terminated', 'archived session reads back as terminated');

      // Idempotent: a re-archive returns the same terminal record and does not error
      // (the second call must NOT re-dispose — proven server-side by the managed
      // `archive_session_disposes_on_the_terminal_transition_only` unit test).
      const reArchived = await client.beta.sessions.archive(s2.id, { betas: BETAS });
      assert.equal(reArchived.status, 'terminated', 're-archive is idempotent');
      pass('archive-path: ARCHIVE reaped the sandbox and tombstoned the session (idempotent)');

      // ── Soak: many create -> turn -> delete cycles stay healthy ───────────
      // Without disposal a per-session sandbox would leak each cycle; the server
      // completing this loop cleanly is the external stability signal.
      for (let i = 0; i < 8; i++) {
        const s = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: 'env_local',
          betas: BETAS,
        });
        await send(client, s.id, `cycle ${i}`);
        const ev = await drain(client.beta.sessions.events.list(s.id, { betas: BETAS }));
        assert.ok(agentTexts(ev).some((t) => t.startsWith('Echo:')), `cycle ${i} ran`);
        const d = await client.beta.sessions.delete(s.id, { betas: BETAS });
        assert.equal(d.type, 'session_deleted', `cycle ${i} deleted`);
      }
      pass('soak: 8 create -> turn -> delete cycles disposed cleanly, server healthy');
    });

    console.log(
      'E2E PASS: managed session sandbox-dispose lifecycle (delete + archive reap the sandbox) via the official @anthropic-ai/sdk.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
