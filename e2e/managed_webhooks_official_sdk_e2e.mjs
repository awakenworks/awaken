// Standard Webhooks differential against the official Anthropic SDK parser.
// The body/header construction is byte-for-byte the Rust awaken-webhook
// contract; no custom verifier participates in the assertions.
// Cause/effect graph: every admitted exact SDK, including a prequalified
// candidate, receives a Rust-compatible Session or Deployment-run lifecycle
// body + fresh signed headers -> unwrap; payload mutation or absent verification
// key -> reject before event handling.
// Decision table: W1={Session bytes,valid key}->accept; W2={Deployment-run
// bytes,valid key}->accept exact public union; W3={tampered bytes,valid key} or
// {valid bytes,missing key}->reject. The product-side outbox test owns actual
// lifecycle emission and retries; this suite owns only official SDK decoding.

import { loadConformanceClients } from '../packages/managed-sdk-oracle/src/conformance/clients.mjs';
import { exerciseOfficialWebhookContract } from './conformance/official_webhook_contract.mjs';
import { pass } from './harness.mjs';

for (const { version, Client: Anthropic } of await loadConformanceClients()) {
  const profile = exerciseOfficialWebhookContract(Anthropic);
  pass(
    `official SDK ${version} webhooks.unwrap rejects tampering/missing keys`
    + (profile.parseUnverified ? ' and parseUnverified parses exact bytes' : ''),
  );
}
pass('all pinned official SDK webhook helpers satisfy their generated surface');
