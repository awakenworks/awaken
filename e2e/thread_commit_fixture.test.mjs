import assert from 'node:assert/strict';
import test from 'node:test';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';

test('the shared ThreadCommit fixture preserves the contract-owned hash decisions', () => {
  // Cause/effect graph: C1 only JSON object insertion order changes; C2 one
  // semantic commit value changes; C3 operation coordinates are explicit; C4
  // the caller-owned inputs are reused after encoding. Effects: E1 C1 retains
  // one payload hash; E2 C2 changes it; E3 C3 is copied exactly; E4 C4 remains
  // byte-for-byte unchanged. Constraints: this fixture mirrors wire encoding
  // only; live HTTP E2Es remain the oracle against Rust `commit_payload_hash`.
  // Decision rules: F1=C1=>E1; F2=C2=>E2; F3=C3=>E3; F4=C4=>E4.
  const claimed = {
    lease: { run_id: 'run-fixture', owner: 'worker:1:incarnation', epoch: 7 },
  };
  const commit = terminalThreadCommitFixture({
    runId: claimed.lease.run_id,
    threadId: 'thread-fixture',
    messageId: 'message-fixture',
    text: 'stable payload',
  });
  const reordered = {
    resume_ticket: commit.resume_ticket,
    events: commit.events,
    state: commit.state,
    messages: commit.messages.map((message) => ({
      content: message.content.map((block) => ({ text: block.text, type: block.type })),
      role: message.role,
      id: message.id,
    })),
    run_fact: {
      phase: commit.run_fact.phase,
      run_id: commit.run_fact.run_id,
    },
    thread_id: commit.thread_id,
  };
  const changed = structuredClone(commit);
  changed.messages[0].content[0].text = 'changed payload';
  const originalClaimed = structuredClone(claimed);
  const originalCommit = structuredClone(commit);
  const input = {
    claimed,
    ordinal: 3,
    expectedThreadVersion: 11,
  };
  const operation = claimedCommitRequestFixture({ ...input, commit });
  const reorderedOperation = claimedCommitRequestFixture({ ...input, commit: reordered });
  const changedOperation = claimedCommitRequestFixture({ ...input, commit: changed });

  assert.equal(operation.operation.payload_hash, reorderedOperation.operation.payload_hash, 'F1/E1');
  assert.notEqual(operation.operation.payload_hash, changedOperation.operation.payload_hash, 'F2/E2');
  assert.deepEqual(
    operation.operation.operation_id,
    { run_id: claimed.lease.run_id, ordinal: 3 },
    'F3/E3 operation id',
  );
  assert.equal(operation.operation.expected_thread_version, 11, 'F3/E3 thread version');
  assert.deepEqual(claimed, originalClaimed, 'F4/E4 claim');
  assert.deepEqual(commit, originalCommit, 'F4/E4 commit');
});
