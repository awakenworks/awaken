// Durable projects: author a project + a per-(project, agent) MCP binding through
// /v1/config/* under AWAKEN_MGMT_DIR, then restart the process and confirm the
// rows survive. Drives the SQLite admin store's ProjectStore path (put/get/list
// project + put/get project-agent binding), which the in-memory projects e2e does
// not reach. Deterministic, hermetic, CI-safe (no live key).
//
// Run: (from e2e/)  node management_projects_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = 38231;
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { _raw: text };
  }
  return { status: res.status, json };
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-projects-durable-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY };
  const upstream = await startUpstream('mcp');
  let ok = false;
  let server;
  try {
    // Boot 1: author two projects + a project-agent MCP binding (durable).
    ({ server } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    let base = `http://127.0.0.1:${PORT}`;

    let r = await req(base, 'PUT', '/v1/config/projects/proj-a', {
      id: 'proj-a',
      workspace_id: 'ws',
      display_name: 'Project A',
      version: 1,
    });
    assert.equal(r.status, 200, `put project: ${JSON.stringify(r.json)}`);
    r = await req(base, 'PUT', '/v1/config/projects/proj-b', {
      id: 'proj-b',
      workspace_id: 'ws',
      display_name: 'Project B',
      version: 1,
    });
    assert.equal(r.status, 200);

    // An invalid project id is fenced (the shared slug rule).
    r = await req(base, 'PUT', '/v1/config/projects/Not_A_Slug', {
      id: 'x',
      workspace_id: 'ws',
      display_name: 'bad',
      version: 1,
    });
    assert.equal(r.status, 422, 'a non-slug project id is rejected');

    // A project-agent binding with no servers selected — exercises the store's
    // put_project_agent path without needing authored MCP defs/credentials.
    r = await req(base, 'PUT', '/v1/config/projects/proj-a/agents/coder/mcp', {
      project_id: 'proj-a',
      agent_id: 'coder',
      mcp_server_ids: [],
      version: 1,
    });
    assert.equal(r.status, 200, `put project-agent mcp: ${JSON.stringify(r.json)}`);

    r = await req(base, 'GET', '/v1/config/projects');
    assert.equal(r.status, 200);
    assert.equal(r.json.length, 2, 'both projects listed');
    pass('authored two projects + a project-agent MCP binding under AWAKEN_MGMT_DIR');

    // Restart over the same dir.
    await stopServer(server);
    ({ server } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    base = `http://127.0.0.1:${PORT}`;

    r = await req(base, 'GET', '/v1/config/projects/proj-a');
    assert.equal(r.status, 200, 'project survived the restart');
    assert.equal(r.json.display_name, 'Project A');
    assert.equal(r.json.workspace_id, 'ws');

    r = await req(base, 'GET', '/v1/config/projects');
    assert.equal(r.json.length, 2, 'both projects survived');

    r = await req(base, 'GET', '/v1/config/projects/proj-a/agents/coder/mcp');
    assert.equal(r.status, 200, 'project-agent binding survived');
    assert.deepEqual(r.json.mcp_server_ids, []);

    // A missing binding is a clean 404.
    r = await req(base, 'GET', '/v1/config/projects/proj-a/agents/ghost/mcp');
    assert.equal(r.status, 404, 'a missing binding is not found');
    pass('projects + bindings survived a restart (durable SQLite admin store)');

    console.log('E2E PASS: durable project + project-agent binding CRUD over the SQLite admin store.');
    ok = true;
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
  process.exitCode = ok ? 0 : 1;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
