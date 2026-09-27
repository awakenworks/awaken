import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { allowManagedToolBoundaries, pass, withServer } from '../harness.mjs';

// One real artifact preparation path shared by SDK runtime and all-operation
// qualification. Dream transcripts are private InlineBytes, never public Files.
export async function verifyGeneratedFileDownload({ Client, fetch, hasGaFiles, port }) {
  // Cause/effect graph: C1 the existing ProbeModel writes the random User bytes
  // through the ordinary gated write tool into the reserved outputs directory;
  // C2 its exact task/approval receipts commit; C3 Runtime harvests a scoped
  // downloadable File; C4 the selected SDK exposes GA Files. Rules F1 C1+C2+C3
  // => Beta returns exact bytes; F2 F1+C4 => GA returns the SAME File bytes.
  // Input-file denial stays with the Files lifecycle owner. No catalog insert,
  // fake downloadable upload, or private physical path can satisfy this test.
  const betas = ['managed-agents-2026-04-01'];
  const content = `AWAKEN_PROBE_ARTIFACT:${randomUUID()}`;
  await withServer('probe', port, async (baseURL) => {
    const client = new Client({ apiKey: 'e2e-dummy', baseURL, ...(fetch ? { fetch } : {}) });
    const session = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas,
    });
    const task = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: content }] }], betas,
    });
    assert.equal(typeof task.data[0]?.id, 'string', 'F1 exact task receipt');
    await allowManagedToolBoundaries({
      client, sessionId: session.id, taskReceiptId: task.data[0].id, betas,
      description: 'F1 generated download artifact',
    });
    const files = [];
    for await (const file of client.beta.files.list({ scope_id: session.id })) files.push(file);
    const artifact = files.find(({ filename }) => filename === 'probe.txt');
    assert.ok(artifact, 'F1 Runtime publishes the actual output');
    assert.equal(artifact.downloadable, true, 'F1 catalog owns download admission');
    assert.equal(await (await client.beta.files.download(artifact.id)).text(), content, 'F1 Beta bytes');
    if (hasGaFiles) {
      assert.equal(await (await client.files.download(artifact.id)).text(), content, 'F2 GA bytes');
    }
    await client.beta.sessions.delete(session.id, { betas });
    pass('official SDK downloads one harvested File through its supported Beta/GA roots');
  });
}
