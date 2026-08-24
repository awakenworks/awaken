// Logical File identity and ownership are workspace-scoped. Drive the durable
// SQLite ownership projection through IAM and the public Files API with two real
// workspace tokens. Physical content deduplication belongs to FileStore and is
// covered by the resource-reclamation E2E rather than duplicated here.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {
  deploymentEnv,
  managedFileUploadForm,
  pass,
  spawnServer,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38216);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const WS_A = 'wrkspc_file_alpha';
const WS_B = 'wrkspc_file_beta';

async function request(
  base: string,
  method: string,
  route: string,
  token: string,
  body?: BodyInit,
  contentType?: string,
): Promise<{ status: number; body: any; text: string }> {
  const headers: Record<string, string> = { authorization: `Bearer ${token}` };
  if (contentType) headers['content-type'] = contentType;
  const response = await fetch(`${base}${route}`, { method, headers, body });
  const text = await response.text();
  let decoded: any = undefined;
  try {
    decoded = text ? JSON.parse(text) : undefined;
  } catch {
    decoded = text;
  }
  return { status: response.status, body: decoded, text };
}

async function mint(base: string, bootstrap: string, workspace: string): Promise<string> {
  const minted = await request(
    base,
    'POST',
    '/v1/config/iam/tokens',
    bootstrap,
    JSON.stringify({ workspace_id: workspace, role: 'workspace_admin' }),
    'application/json',
  );
  assert.equal(minted.status, 201, minted.text);
  return minted.body.token;
}

async function upload(base: string, token: string, bytes: string): Promise<string> {
  const uploaded = await request(
    base,
    'POST',
    '/v1/files',
    token,
    managedFileUploadForm(bytes, 'shared.txt'),
  );
  assert.equal(uploaded.status, 200, uploaded.text);
  return uploaded.body.id;
}

async function main(): Promise<void> {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-file-owners-'));
  const env = deploymentEnv(dir, {
    identityMode: 'self-managed',
    iamWorkspaces: [WS_A, WS_B],
    controlSealKey: SEAL_KEY,
  });
  const { server, baseUrl } = spawnServer('management', PORT, env);
  try {
    await waitForPort(PORT, 180_000, server);
    const bootstrap = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();
    const tokenA = await mint(baseUrl, bootstrap, WS_A);
    const tokenB = await mint(baseUrl, bootstrap, WS_B);

    const fileA = await upload(baseUrl, tokenA, 'same-content-across-workspaces');
    const fileB = await upload(baseUrl, tokenB, 'same-content-across-workspaces');

    // Workspace ownership decision table:
    // C1 caller owns the logical File, C2 another Workspace uploaded equal bytes,
    // C3 owner A has deleted its File. Effects are E1 authorized reads succeed,
    // E2 cross-Workspace reads fail closed, E3 equal bytes retain distinct logical
    // identities, and E4 deleting A does not revoke B.
    //
    // | Rule | C1 owns | C2 equal bytes elsewhere | C3 A deleted | Effect |
    // |---|---|---|---|---|
    // | D1 | T | T | F | E1 + E3 |
    // | D2 | F | T | F | E2 |
    // | D3 | T(B) | T | T | E4 |
    assert.notEqual(fileB, fileA, 'equal bytes keep distinct workspace-owned logical identities');
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileA}`, tokenA)).status, 200, 'D1');
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileB}`, tokenB)).status, 200, 'D1');
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileA}`, tokenB)).status, 404, 'D2');
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileB}`, tokenA)).status, 404, 'D2');

    const deleteA = await request(baseUrl, 'DELETE', `/v1/files/${fileA}`, tokenA);
    assert.equal(deleteA.status, 200, deleteA.text);
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileB}`, tokenB)).status, 200);
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileA}`, tokenA)).status, 404);
    pass('revoking one durable owner preserves the other workspace logical File');

    const duplicateDelete = await request(baseUrl, 'DELETE', `/v1/files/${fileA}`, tokenA);
    assert.equal(duplicateDelete.status, 404, duplicateDelete.text);
    const deleteB = await request(baseUrl, 'DELETE', `/v1/files/${fileB}`, tokenB);
    assert.equal(deleteB.status, 200, deleteB.text);
    assert.equal((await request(baseUrl, 'GET', `/v1/files/${fileB}`, tokenB)).status, 404);
    pass('each workspace can revoke only its own logical File; duplicate revoke fails closed');

    console.log('FILE WORKSPACE OWNERSHIP TS API E2E PASS.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('FILE WORKSPACE OWNERSHIP TS API E2E FAIL:', error);
  process.exitCode = 1;
});
