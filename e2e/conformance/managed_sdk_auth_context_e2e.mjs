// Exact supported Anthropic SDKs × real self-managed authentication boundary.
// This is deliberately separate from synthetic SDK transport tests and the
// management token-lifecycle suite: one real call must compose both sides.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import {
  loadConformanceClients,
  projectsWorkspaceResponseContext,
} from '../../packages/managed-sdk-oracle/src/conformance/clients.mjs';
import {
  cleanupFixtureTree,
  deploymentEnv,
  pass,
  spawnServer,
  stopServer,
  waitForPort,
} from '../harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38198);
const SEAL_KEY = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef';
const CLIENTS = await loadConformanceClients();

function recordUniqueRequestID(seen, value, label) {
  assert.match(value ?? '', /^req_[0-9a-f]{32}$/u, `${label}: generated request-id`);
  assert.ok(!seen.has(value), `${label}: request-id is globally unique`);
  seen.add(value);
  return value;
}

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-sdk-auth-context-'));
  const environment = deploymentEnv(root, {
    identityMode: 'self-managed',
    controlSealKey: SEAL_KEY,
  });
  let server;
  try {
    const running = spawnServer('management', PORT, environment);
    server = running.server;
    await waitForPort(PORT, 900_000, server);
    const token = fs.readFileSync(path.join(root, 'admin-token'), 'utf8').trim();
    const workspace = fs.readFileSync(path.join(root, 'platform-workspace-id'), 'utf8').trim();
    const requestIDs = new Set();
    const deniedMarker = `sdk-authz-denied-${Date.now()}`;
    const mint = await fetch(`${running.baseUrl}/v1/config/iam/tokens`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${token}`,
        'content-type': 'application/json',
      },
      body: JSON.stringify({
        workspace_id: workspace,
        role: 'workspace_restricted_developer',
      }),
    });
    const issued = await mint.json();
    assert.equal(mint.status, 201, 'mint restricted fixture');
    assert.equal(typeof issued.token, 'string', 'restricted fixture token');

    // Test design: every_official_sdk_composes_with_the_real_authentication_boundary
    //
    // Cause/effect graph:
    // exact supported SDK + invalid x-api-key
    //   -> real self-managed IAM guard
    //   -> unscoped canonical 401
    //   -> exact SDK AuthenticationError + fresh request-id + no Workspace;
    // exact supported SDK + valid Bearer token
    //   -> resolved Workspace scope
    //   -> 200 page + fresh request-id + exact Workspace;
    // exact supported SDK + dynamic credential provider
    //   -> OAuth + Managed capabilities on one real request
    //   -> one provider resolution + the same scoped 200 contract;
    // exact supported SDK + in-memory user-OAuth config + private token file
    //   -> config-selected base URL and Workspace header
    //   -> the same real scoped 200 contract;
    // exact supported SDK + read-only Bearer token + Vault mutation
    //   -> resolved Workspace scope + denied action
    //   -> exact SDK PermissionDeniedError + exact Workspace + no side effect.
    //
    // This closes the cross-layer edge that synthetic SDK response fixtures and
    // raw HTTP IAM tests cannot prove independently.
    //
    // Decision table:
    // | credential | identity | action | SDK result            | Workspace |
    // | invalid    | absent   | read   | AuthenticationError   | absent    |
    // | restricted | resolved | write  | PermissionDeniedError | exact     |
    // | admin      | resolved | read   | Vault page            | exact     |
    // | provider   | resolved | read   | Vault page, one lookup| exact     |
    // | config     | resolved | read   | Vault page, file token| exact     |
    for (const { role, version, Client } of CLIENTS) {
      const label = `${role}:${version}`;
      const projectsWorkspace = projectsWorkspaceResponseContext(version);
      const invalid = new Client({
        apiKey: `invalid-${version}`, // awaken-allow: secret
        baseURL: running.baseUrl,
        maxRetries: 0,
      });
      await assert.rejects(
        () => invalid.beta.vaults.list({ betas: BETAS }),
        (error) => {
          assert.equal(error.constructor, Client.AuthenticationError, `${label}: exact class`);
          assert.equal(error.status, 401, `${label}: status`);
          assert.equal(error.type, 'authentication_error', `${label}: promoted type`);
          assert.equal(error.error?.type, 'error', `${label}: canonical envelope`);
          assert.equal(
            error.error?.error?.type,
            'authentication_error',
            `${label}: canonical nested type`,
          );
          const requestID = recordUniqueRequestID(
            requestIDs,
            error.headers?.get('request-id'),
            `${label}: unauthenticated response`,
          );
          assert.equal(error.requestID, requestID, `${label}: promoted request-id`);
          assert.equal(
            error.headers?.get('anthropic-workspace-id'),
            null,
            `${label}: unauthenticated response cannot disclose a Workspace`,
          );
          assert.equal(
            'workspaceID' in error,
            projectsWorkspace,
            `${label}: reviewed error Workspace capability`,
          );
          if (projectsWorkspace) {
            assert.equal(error.workspaceID, null, `${label}: absent Workspace remains null`);
          }
          return true;
        },
      );

      const authorized = new Client({
        apiKey: null,
        authToken: token,
        baseURL: running.baseUrl,
        maxRetries: 0,
      });
      const listed = await authorized.beta.vaults.list({ betas: BETAS }).withResponse();
      const requestID = recordUniqueRequestID(
        requestIDs,
        listed.response.headers.get('request-id'),
        `${label}: authenticated response`,
      );
      assert.equal(listed.request_id, requestID, `${label}: withResponse request-id`);
      assert.equal(
        listed.response.headers.get('anthropic-workspace-id'),
        workspace,
        `${label}: exact authenticated Workspace`,
      );
      assert.equal(Array.isArray(listed.data.data), true, `${label}: decoded Vault page`);
      assert.equal(
        'workspace_id' in listed,
        projectsWorkspace,
        `${label}: reviewed success Workspace capability`,
      );
      if (projectsWorkspace) {
        assert.equal(listed.workspace_id, workspace, `${label}: promoted Workspace`);
      }

      const providerCalls = [];
      const providerAuthorized = new Client({
        apiKey: null,
        authToken: null,
        credentials: async (options) => {
          providerCalls.push(options ?? null);
          return { token, expiresAt: null };
        },
        baseURL: running.baseUrl,
        maxRetries: 0,
      });
      const provided = await providerAuthorized.beta.vaults.list({ betas: BETAS }).withResponse();
      assert.deepEqual(providerCalls, [null], `${label}: provider resolves once`);
      const providerRequestID = recordUniqueRequestID(
        requestIDs,
        provided.response.headers.get('request-id'),
        `${label}: dynamic-provider response`,
      );
      assert.equal(provided.request_id, providerRequestID, `${label}: provider request-id`);
      assert.equal(
        provided.response.headers.get('anthropic-workspace-id'),
        workspace,
        `${label}: provider retains the authenticated Workspace`,
      );
      assert.equal(Array.isArray(provided.data.data), true, `${label}: provider decodes Vault page`);
      assert.equal(
        'workspace_id' in provided,
        projectsWorkspace,
        `${label}: reviewed provider Workspace capability`,
      );
      if (projectsWorkspace) {
        assert.equal(provided.workspace_id, workspace, `${label}: promoted provider Workspace`);
      }

      const credentialsPath = path.join(root, `sdk-config-${version}.json`);
      fs.writeFileSync(credentialsPath, JSON.stringify({
        version: '1.0',
        type: 'oauth_token',
        access_token: token,
      }), { mode: 0o600 });
      const configured = new Client({
        apiKey: null,
        authToken: null,
        config: {
          authentication: {
            type: 'user_oauth',
            credentials_path: credentialsPath,
          },
          base_url: running.baseUrl,
          workspace_id: workspace,
        },
        baseURL: null,
        maxRetries: 0,
      });
      assert.equal(configured.baseURL, running.baseUrl, `${label}: config selects the API host`);
      const configPage = await configured.beta.vaults.list({ betas: BETAS }).withResponse();
      const configRequestID = recordUniqueRequestID(
        requestIDs,
        configPage.response.headers.get('request-id'),
        `${label}: configured-credential response`,
      );
      assert.equal(configPage.request_id, configRequestID, `${label}: config request-id`);
      assert.equal(
        configPage.response.headers.get('anthropic-workspace-id'),
        workspace,
        `${label}: config Workspace agrees with authenticated scope`,
      );
      assert.equal(Array.isArray(configPage.data.data), true, `${label}: config decodes Vault page`);
      assert.equal(
        'workspace_id' in configPage,
        projectsWorkspace,
        `${label}: reviewed config Workspace capability`,
      );
      if (projectsWorkspace) {
        assert.equal(configPage.workspace_id, workspace, `${label}: promoted config Workspace`);
      }

      const restricted = new Client({
        apiKey: null,
        authToken: issued.token,
        baseURL: running.baseUrl,
        maxRetries: 0,
      });
      await assert.rejects(
        () => restricted.beta.vaults.create({
          display_name: `${deniedMarker}-${version}`,
          betas: BETAS,
        }),
        (error) => {
          assert.equal(
            error.constructor,
            Client.PermissionDeniedError,
            `${label}: exact permission class`,
          );
          assert.equal(error.status, 403, `${label}: permission status`);
          assert.equal(error.type, 'permission_error', `${label}: promoted permission type`);
          assert.equal(error.error?.type, 'error', `${label}: permission envelope`);
          assert.equal(
            error.error?.error?.type,
            'permission_error',
            `${label}: nested permission type`,
          );
          const deniedRequestID = recordUniqueRequestID(
            requestIDs,
            error.headers?.get('request-id'),
            `${label}: denied response`,
          );
          assert.equal(error.requestID, deniedRequestID, `${label}: denied request-id`);
          assert.equal(
            error.headers?.get('anthropic-workspace-id'),
            workspace,
            `${label}: denied response retains the authenticated Workspace`,
          );
          assert.equal(
            'workspaceID' in error,
            projectsWorkspace,
            `${label}: reviewed denied Workspace capability`,
          );
          if (projectsWorkspace) {
            assert.equal(error.workspaceID, workspace, `${label}: promoted denied Workspace`);
          }
          return true;
        },
      );
      pass(`${label}: real self-managed static/provider/config 401/403/200 response context`);
    }

    const current = CLIENTS.find(({ role }) => role === 'current_oracle');
    assert.ok(current, 'current official SDK client');
    const verification = await new current.Client({
      apiKey: null,
      authToken: token,
      baseURL: running.baseUrl,
      maxRetries: 0,
    }).beta.vaults.list({ betas: BETAS }).withResponse();
    const verificationRequestID = recordUniqueRequestID(
      requestIDs,
      verification.response.headers.get('request-id'),
      'post-denial verification',
    );
    assert.equal(
      verification.request_id,
      verificationRequestID,
      'post-denial verification promotes the request identity',
    );
    assert.equal(
      verification.response.headers.get('anthropic-workspace-id'),
      workspace,
      'post-denial verification retains the authenticated Workspace',
    );
    if (projectsWorkspaceResponseContext(current.version)) {
      assert.equal(
        verification.workspace_id,
        workspace,
        'post-denial verification promotes the authenticated Workspace',
      );
    }
    assert.ok(
      verification.data.data.every(({ display_name: name }) => !name.startsWith(deniedMarker)),
      'denied mutations have no persisted side effect',
    );
    assert.equal(
      requestIDs.size,
      CLIENTS.length * 5 + 1,
      'every observed SDK response owns one request identity',
    );
  } finally {
    if (server) await stopServer(server);
    cleanupFixtureTree(root);
  }
}

await main();
console.log('E2E PASS: every supported official SDK composes with real authentication context.');
