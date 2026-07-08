// Management-plane vault/credential e2e driven by the **official** Anthropic
// TypeScript SDK (`@anthropic-ai/sdk` `beta.vaults.*`). It spawns
// awaken-server-local in `management` mode and exercises the vault/credential
// front door through the SDK client, so any wire-shape drift from the official
// `BetaManagedAgentsVault` / `BetaManagedAgentsCredential` types surfaces as an
// SDK deserialization error here.
//
// Run: (from e2e/)  npm install && node management_vaults_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38130, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Create a vault through the SDK; the SDK parses BetaManagedAgentsVault.
      const vault = await client.beta.vaults.create({
        display_name: 'E2E vault',
        metadata: { team: 'core' },
        betas: BETAS,
      });
      assert.equal(vault.type, 'vault');
      assert.equal(vault.display_name, 'E2E vault');
      assert.equal(vault.metadata.team, 'core');
      assert.ok(vault.id.startsWith('vlt_'), `vault id: ${vault.id}`);
      pass('beta.vaults.create -> BetaManagedAgentsVault');

      // Enter an environment_variable credential; secret_value is write-only.
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'environment_variable',
        secret_name: 'ANTHROPIC_API_KEY',
        secret_value: 'sk-e2e-secret', // awaken-allow: secret
        networking: { type: 'unrestricted' },
        betas: BETAS,
      });
      assert.equal(cred.type, 'vault_credential');
      assert.equal(cred.vault_id, vault.id);
      assert.equal(cred.auth.type, 'environment_variable');
      assert.equal(cred.auth.secret_name, 'ANTHROPIC_API_KEY');
      assert.equal(cred.auth.networking.type, 'unrestricted');
      // The secret is never echoed on the SDK-parsed object.
      assert.ok(!JSON.stringify(cred).includes('sk-e2e-secret'), 'secret must not be echoed');
      pass('beta.vaults.credentials.create -> secret-free BetaManagedAgentsCredential');

      // Retrieve it back (secret-free).
      const got = await client.beta.vaults.credentials.retrieve(cred.id, {
        vault_id: vault.id,
        betas: BETAS,
      });
      assert.equal(got.id, cred.id);
      assert.equal(got.vault_id, vault.id);
      pass('beta.vaults.credentials.retrieve -> same credential, secret-free');

      // Validate: an env-var credential has no upstream to probe -> `unknown`.
      const validation = await client.beta.vaults.credentials.mcpOAuthValidate(cred.id, {
        vault_id: vault.id,
        betas: BETAS,
      });
      assert.equal(validation.type, 'vault_credential_validation');
      assert.equal(validation.status, 'unknown');
      pass('beta.vaults.credentials.mcpOAuthValidate -> BetaManagedAgentsCredentialValidation{unknown}');

      // Retrieve the vault by id, too.
      const back = await client.beta.vaults.retrieve(vault.id, { betas: BETAS });
      assert.equal(back.id, vault.id);
      pass('beta.vaults.retrieve -> BetaManagedAgentsVault');

      // Delete the vault; a subsequent retrieve must 404.
      const deleted = await client.beta.vaults.delete(vault.id, { betas: BETAS });
      assert.equal(deleted.type, 'vault_deleted');
      assert.equal(deleted.id, vault.id);
      await assert.rejects(
        () => client.beta.vaults.retrieve(vault.id, { betas: BETAS }),
        (err) => {
          assert.equal(err.status, 404);
          return true;
        },
      );
      pass('beta.vaults.delete -> BetaManagedAgentsDeletedVault; retrieve 404s after');
    });

    console.log('E2E PASS: management vault/credential surface round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
