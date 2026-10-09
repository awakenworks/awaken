// Workspace narrowing of the content-capture ceiling (ADR-0050 D3, amended
// 2026-10-09): the deployment configuration holds the binding ceiling in a
// self-hosted deployment and a Workspace may only tighten it. This drives the
// real binary: the authored narrowing changes what a real attributed Session
// turn persists, and a wider narrowing is rejected.
//
// Cause graph / decision table:
//   C1 typed deployment ceiling=full; C2 telemetry consent granted for the
//   subject; C3 the Workspace narrowing authored through /v1/config/telemetry.
//
// | Rule | C1 | C2 | C3 (narrowing) | Effect |
// |---|---|---|---|---|
// | N1 | full | Y | unset | an attributed turn persists subject-owned content |
// | N2 | full | Y | structured | the next attributed turn persists no content |
// | N3 | full | Y | (stale revision) | 409, the stored narrowing is unchanged |
// | N4 | full | Y | cleared | content capture resumes on the next turn |
//
// A narrowing above the deployment ceiling cannot be authored: with the
// ceiling at full there is no wider level, so N3 covers the CAS edge and the
// Rust route table covers the wider-than-ceiling 400.
//
// Run: (from e2e/)  node management_workspace_capture_narrowing_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  USER_PROFILES_BETA,
  deploymentEnv,
  withRealServer,
  waitForSessionEventReceipt,
  pass,
} from './harness.mjs';
import { sqliteRows } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const SUBJECT = 'dsub_ws_narrowing';
const PROFILE_HEADERS = { 'anthropic-beta': USER_PROFILES_BETA };

async function putTelemetry(base, body) {
  const res = await fetch(`${base}/v1/config/telemetry`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  return { status: res.status, body: await res.json() };
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-ws-capture-'));
  const env = deploymentEnv(directory, {
    identityMode: 'no-login',
    fields: { content_capture: 'full' },
  });
  const captured = () =>
    sqliteRows(
      path.join(directory, 'captured_content.db'),
      'SELECT content FROM coordinator_data_capture_captured WHERE subject = ?',
      SUBJECT,
    ).length;
  try {
    await withRealServer(
      'echo',
      Number(process.env.E2E_PORT),
      async (base) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
        const consent = await fetch(`${base}/v1/user_profiles/${SUBJECT}/consent`, {
          method: 'POST',
          headers: { ...PROFILE_HEADERS, 'content-type': 'application/json' },
          body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
        });
        assert.equal(consent.status, 200, `consent status ${consent.status}`);

        const initial = await (await fetch(`${base}/v1/config/telemetry`)).json();
        assert.equal(initial.revision, 0, JSON.stringify(initial));
        assert.equal(initial.ceiling, 'full');
        assert.equal(initial.effective, 'full');

        const session = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: 'env_local',
          betas: BETAS,
        });
        const turn = async (text) => {
          const receipt = await client.beta.sessions.events.send(
            session.id,
            { betas: BETAS, events: [{ type: 'user.message', content: [{ type: 'text', text }] }] },
            { headers: { 'anthropic-user-profile-id': SUBJECT } },
          );
          await waitForSessionEventReceipt(
            client,
            session.id,
            receipt.data[0]?.id,
            BETAS,
            ({ delta }) => delta.some((event) => event.type === 'agent.message'),
            `turn "${text}" reaches its committed reply`,
          );
        };

        await turn('captured under the deployment ceiling');
        const afterOpen = captured();
        assert.ok(afterOpen > 0, 'N1 an attributed turn persists subject-owned content');
        pass('N1 unset Workspace narrowing inherits the full deployment ceiling');

        const narrowed = await putTelemetry(base, {
          expected_revision: 0,
          content_capture: 'structured',
        });
        assert.equal(narrowed.status, 200, JSON.stringify(narrowed.body));
        assert.equal(narrowed.body.effective, 'structured');
        await turn('not captured under the Workspace narrowing');
        assert.equal(captured(), afterOpen, 'N2 the narrowed Workspace persists no content');
        pass('N2 Workspace narrowing to structured stops content capture on the next Run');

        const stale = await putTelemetry(base, { expected_revision: 0 });
        assert.equal(stale.status, 409, JSON.stringify(stale.body));
        const current = await (await fetch(`${base}/v1/config/telemetry`)).json();
        assert.equal(current.content_capture, 'structured', 'N3 stale write changed nothing');
        pass('N3 a stale narrowing write is a conflict');

        const cleared = await putTelemetry(base, { expected_revision: 1 });
        assert.equal(cleared.status, 200, JSON.stringify(cleared.body));
        assert.equal(cleared.body.effective, 'full');
        await turn('captured again after clearing the narrowing');
        assert.ok(captured() > afterOpen, 'N4 capture resumes once the narrowing is cleared');
        pass('N4 clearing the narrowing restores the deployment ceiling');
      },
      { mode: 'management', extraEnv: env },
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
