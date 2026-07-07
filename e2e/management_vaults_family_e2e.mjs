// The **rest of** the Managed vault/credential family, driven by the official
// Anthropic TypeScript SDK (`@anthropic-ai/sdk` `beta.vaults.*`): List / Update /
// Archive on both the vault and the credential, plus Delete credential. The
// happy-path create/retrieve/validate/delete-vault flow is covered by
// `management_vaults_e2e.mjs`; this file exercises the endpoints that were
// missing so any wire-shape drift from the official `PageCursor` /
// `BetaManagedAgentsVault` / `BetaManagedAgentsCredential` /
// `BetaManagedAgentsDeletedCredential` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_vaults_family_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

// Collect every item the SDK auto-paginator yields for a list call. Exercises
// the `PageCursor` decode (`data` / `has_more` / `next_page`) end to end.
async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withServer('management', 38132, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // -- Vault List --------------------------------------------------------
      const vaultA = await client.beta.vaults.create({
        display_name: 'A',
        metadata: { a: '1', keep: 'x' },
        betas: BETAS,
      });
      const vaultB = await client.beta.vaults.create({ display_name: 'B', betas: BETAS });
      const vaultIds = (await drain(client.beta.vaults.list({ betas: BETAS }))).map((v) => v.id);
      assert.ok(vaultIds.includes(vaultA.id) && vaultIds.includes(vaultB.id), 'list returns both vaults');
      pass('beta.vaults.list -> PageCursor<BetaManagedAgentsVault>');

      // -- Vault Update (rename + metadata patch) ----------------------------
      const updatedVault = await client.beta.vaults.update(vaultA.id, {
        display_name: 'A-renamed',
        metadata: { a: null, b: '2' }, // delete `a`, add `b`, keep `keep`
        betas: BETAS,
      });
      assert.equal(updatedVault.display_name, 'A-renamed');
      assert.equal(updatedVault.metadata.b, '2');
      assert.equal(updatedVault.metadata.keep, 'x');
      assert.ok(!('a' in updatedVault.metadata), 'metadata key `a` was deleted by the null patch');
      pass('beta.vaults.update -> rename + metadata patch');

      // -- Credential List ---------------------------------------------------
      const c1 = await client.beta.vaults.credentials.create(vaultA.id, {
        type: 'environment_variable',
        secret_name: 'K1',
        secret_value: 'sk-one', // awaken-allow: secret
        networking: { type: 'unrestricted' },
        betas: BETAS,
      });
      const c2 = await client.beta.vaults.credentials.create(vaultA.id, {
        type: 'environment_variable',
        secret_name: 'K2',
        secret_value: 'sk-two', // awaken-allow: secret
        networking: { type: 'unrestricted' },
        betas: BETAS,
      });
      const credIds = (await drain(client.beta.vaults.credentials.list(vaultA.id, { betas: BETAS }))).map(
        (c) => c.id,
      );
      assert.deepEqual(credIds, [c1.id, c2.id], 'credentials list in ascending-id order');
      pass('beta.vaults.credentials.list -> PageCursor<BetaManagedAgentsCredential>');

      // -- Credential Update (patch + re-seal + networking) ------------------
      const updatedCred = await client.beta.vaults.credentials.update(c1.id, {
        vault_id: vaultA.id,
        auth: {
          type: 'environment_variable',
          secret_value: 'sk-one-rotated', // awaken-allow: secret
          networking: { type: 'limited', allowed_hosts: ['api.example.com'] },
        },
        display_name: 'renamed cred',
        metadata: { team: 'core' },
        betas: BETAS,
      });
      assert.equal(updatedCred.auth.type, 'environment_variable');
      assert.equal(updatedCred.auth.networking.type, 'limited');
      assert.deepEqual(updatedCred.auth.networking.allowed_hosts, ['api.example.com']);
      assert.equal(updatedCred.display_name, 'renamed cred');
      assert.equal(updatedCred.metadata.team, 'core');
      assert.ok(!JSON.stringify(updatedCred).includes('sk-one-rotated'), 'rotated secret is never echoed');
      pass('beta.vaults.credentials.update -> networking + metadata + secret re-seal (secret-free)');

      // -- Credential Archive (soft-delete, hidden from default list) --------
      const archivedCred = await client.beta.vaults.credentials.archive(c2.id, {
        vault_id: vaultA.id,
        betas: BETAS,
      });
      assert.ok(archivedCred.archived_at, 'archived credential carries archived_at');
      const activeCreds = (await drain(client.beta.vaults.credentials.list(vaultA.id, { betas: BETAS }))).map(
        (c) => c.id,
      );
      assert.deepEqual(activeCreds, [c1.id], 'archived credential excluded from the default list');
      const allCreds = (
        await drain(client.beta.vaults.credentials.list(vaultA.id, { include_archived: true, betas: BETAS }))
      ).map((c) => c.id);
      assert.deepEqual(allCreds.sort(), [c1.id, c2.id].sort(), 'include_archived=true returns both');
      pass('beta.vaults.credentials.archive -> soft-delete, include_archived list knob');

      // -- Credential Delete (hard-delete, receipt) --------------------------
      const deletedCred = await client.beta.vaults.credentials.delete(c1.id, {
        vault_id: vaultA.id,
        betas: BETAS,
      });
      assert.equal(deletedCred.type, 'vault_credential_deleted');
      assert.equal(deletedCred.id, c1.id);
      await assert.rejects(
        () => client.beta.vaults.credentials.retrieve(c1.id, { vault_id: vaultA.id, betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('beta.vaults.credentials.delete -> BetaManagedAgentsDeletedCredential; retrieve 404s after');

      // -- Credential Update: mcp_oauth refresh scheme kept WITHOUT resending
      //    the client secret (the SDK's update type makes client_secret optional;
      //    the create type requires it — this must not 400). ------------------
      const oauth = await client.beta.vaults.credentials.create(vaultA.id, {
        type: 'mcp_oauth',
        mcp_server_url: 'https://mcp.example.com/sse',
        access_token: 'at', // awaken-allow: secret
        refresh: {
          client_id: 'cli',
          refresh_token: 'rt', // awaken-allow: secret
          token_endpoint: 'https://auth.example.com/token',
          token_endpoint_auth: { type: 'client_secret_basic', client_secret: 'cs-orig' }, // awaken-allow: secret
        },
        betas: BETAS,
      });
      const oauthUpdated = await client.beta.vaults.credentials.update(oauth.id, {
        vault_id: vaultA.id,
        auth: {
          type: 'mcp_oauth',
          // No client_secret: keep the sealed one, only change the scheme.
          refresh: { token_endpoint_auth: { type: 'client_secret_post' } },
        },
        betas: BETAS,
      });
      assert.equal(oauthUpdated.auth.refresh.token_endpoint_auth.type, 'client_secret_post');
      assert.ok(!JSON.stringify(oauthUpdated).includes('cs-orig'), 'client secret never echoed');
      pass('beta.vaults.credentials.update -> refresh scheme switch without resending client_secret');

      // -- Vault Archive (soft-delete, hidden from default list) -------------
      const archivedVault = await client.beta.vaults.archive(vaultB.id, { betas: BETAS });
      assert.ok(archivedVault.archived_at, 'archived vault carries archived_at');
      const activeVaultIds = (await drain(client.beta.vaults.list({ betas: BETAS }))).map((v) => v.id);
      assert.ok(!activeVaultIds.includes(vaultB.id), 'archived vault excluded from the default list');
      assert.ok(activeVaultIds.includes(vaultA.id), 'the active vault is still listed');
      const allVaultIds = (await drain(client.beta.vaults.list({ include_archived: true, betas: BETAS }))).map(
        (v) => v.id,
      );
      assert.ok(allVaultIds.includes(vaultB.id), 'include_archived=true returns the archived vault');
      pass('beta.vaults.archive -> soft-delete, include_archived list knob');
    });

    console.log('E2E PASS: the vault List/Update/Archive/Delete family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
