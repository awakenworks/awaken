// Cross-language E2E wire fixture for the contract-owned
// `awaken_agent_contract::commit_payload_hash`. It only encodes caller-supplied
// ThreadCommit bytes and explicit operation coordinates; it owns no claim,
// Run/Thread, terminal, retry, compatibility, or validation policy.

import { createHash } from 'node:crypto';

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.keys(value).sort().map((key) =>
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function commitOperation({ commit, runId, ordinal, expectedThreadVersion }) {
  const version = Buffer.from('awaken.thread-commit.v1');
  const payload = Buffer.from(canonicalJson(commit));
  const versionLength = Buffer.alloc(8);
  versionLength.writeBigUInt64LE(BigInt(version.length));
  const payloadLength = Buffer.alloc(8);
  payloadLength.writeBigUInt64LE(BigInt(payload.length));
  const hash = createHash('sha256')
    .update(versionLength).update(version).update(payloadLength).update(payload).digest('hex');
  return {
    operation_id: { run_id: runId, ordinal },
    expected_thread_version: expectedThreadVersion,
    payload_hash: `sha256:${hash}`,
    commit,
  };
}

/**
 * @param {{
 *   runId: string,
 *   threadId: string,
 *   messageId: string,
 *   text: string,
 * }} input
 */
export function terminalThreadCommitFixture({ runId, threadId, messageId, text }) {
  return {
    thread_id: threadId,
    run_fact: { run_id: runId, phase: { Ended: 'NaturalEnd' } },
    messages: [
      { id: messageId, role: 'Assistant', content: [{ type: 'text', text }] },
    ],
    state: [],
    events: [],
    resume_ticket: null,
  };
}

/**
 * @param {{
 *   claimed: { lease: { run_id: string, owner: string, epoch: number } },
 *   commit: Record<string, unknown>,
 *   ordinal: number,
 *   expectedThreadVersion: number,
 * }} input
 */
export function claimedCommitRequestFixture({
  claimed,
  commit,
  ordinal,
  expectedThreadVersion,
}) {
  const runId = claimed.lease.run_id;
  return {
    claim: {
      run_id: runId,
      owner: claimed.lease.owner,
      epoch: claimed.lease.epoch,
    },
    operation: commitOperation({ commit, runId, ordinal, expectedThreadVersion }),
  };
}
