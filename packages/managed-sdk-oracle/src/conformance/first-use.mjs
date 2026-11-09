// Focused Key first use over the same official clients and response oracle.
// This is not a full all-operation or release qualification substitute.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { loadQualifiedClients, qualifiedClient } from './clients.mjs';
import { extractOperationsFromPackageRoot } from '../extract-operations.mjs';
import { projectWireResponseContracts } from './wire-response-projection.mjs';
import { extractResponseContractsFromPackageRoot } from '../extract-wire-contracts.mjs';
import { managedSdkReceipt, assertResponseContract } from '../../../../e2e/conformance/managed_sdk_operation_receipts.mjs';
import { assertManagedResponseContext } from './deployed-sweep.mjs';

const scope = JSON.parse(fs.readFileSync(new URL('../../config/scope.json', import.meta.url)));
const families = ['agents', 'environments', 'models', 'sessions'];
const operations = families.map(family => `beta.${family}.list`);

export async function exerciseWorkspaceKeyFirstUse({
  baseURL, apiKey, workspaceID, clients, fetchImpl = globalThis.fetch,
  requireNonEmpty = [],
}) {
  const base = new URL(baseURL);
  assert.ok(base.pathname === '/' && !base.search && !base.hash, 'use the service origin as SDK base URL');
  assert.ok(apiKey && workspaceID, 'a bound Key and its expected Workspace are required');
  const anchors = await loadQualifiedClients();
  const current = qualifiedClient(anchors, 'current_oracle');
  const selected = clients ?? anchors;
  const currentOperations = extractOperationsFromPackageRoot(current.root, scope).operations;
  const currentResponseContracts = extractResponseContractsFromPackageRoot(current.root, scope, operations);
  assert.ok(selected.length > 0, 'one or more exact official clients are required');
  const receipts = [];
  const seenRequestIDs = new Set();
  for (const { Client, root, version } of selected) {
    const selectedOperations = extractOperationsFromPackageRoot(root, scope).operations.filter(({ id }) => operations.includes(id));
    const contracts = projectWireResponseContracts({ selectedOperations, currentOperations,
      selectedResponseContracts: extractResponseContractsFromPackageRoot(root, scope, operations), currentResponseContracts });
    const managedBetas = selectedOperations.find(({ id }) => id === 'beta.agents.list')?.betas;
    assert.ok(managedBetas?.length > 0, 'the selected SDK must declare its Managed Agents capability');
    for (const style of ['x-api-key', 'bearer']) {
      const observed = new Set();
      let observation;
      const client = new Client({ baseURL: base.origin, maxRetries: 0, timeout: 15000,
        ...(style === 'x-api-key' ? { apiKey } : { apiKey: null, authToken: apiKey }),
        fetch: async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          assert.equal(url.origin, base.origin, 'no hidden SDK origin rewrite');
          assert.ok(families.some(family => url.pathname === `/v1/${family}`), 'flat SDK path without Workspace rewrite');
          assert.equal(request.headers.has('anthropic-workspace-id'), false, 'bound Key must work without a Workspace request Header');
          const response = await fetchImpl(input, init);
          observation = { request, response: response.clone(), family: url.pathname.slice('/v1/'.length) };
          return response;
        },
      });
      const counts = {};
      for (const family of families) {
        // Models may share the GA URL without an automatic Beta selector in a
        // newer SDK. Negotiate this probe's existing Managed Agents context
        // through the standard SDK argument, derived from its own declaration.
        observation = undefined;
        const page = await client.beta[family].list({ limit: 1, betas: managedBetas });
        assert.equal(observation?.family, family, 'the real SDK transport observed this family');
        const { request, response } = observation;
        // Validate after the SDK transport: SDK 1.6 wraps errors thrown inside
        // a custom fetch in APIConnectionError, including response assertions.
        const receipt = await managedSdkReceipt(request, undefined, response);
        assert.equal(receipt.sdkVersion, version, 'exact installed official SDK');
        assertResponseContract(receipt.responseShape, contracts[`beta.${family}.list`], `${version}/${style}/${family}`);
        assertManagedResponseContext({ requestID: response.headers.get('request-id'),
          workspaceID: response.headers.get('anthropic-workspace-id'), expectedWorkspaceID: workspaceID,
          seenRequestIDs, label: `${version}/${style}/${family}` });
        observed.add(family);
        assert.ok(Array.isArray(page.data), 'SDK decoded the declared page');
        if (requireNonEmpty.includes(family)) assert.ok(page.data.length > 0, `${family}: known fixture cannot disappear into an empty page`);
        counts[family] = page.data.length;
      }
      assert.deepEqual([...observed].sort(), [...families].sort(), 'all first-use families traversed the real SDK transport');
      receipts.push({ sdk_version: version, authentication: style, typed_pages: true, workspace_context: true, counts });
    }
  }
  return receipts;
}
