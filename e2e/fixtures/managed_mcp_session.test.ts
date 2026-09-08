import assert from 'node:assert/strict';
import test from 'node:test';
import type Anthropic from '@anthropic-ai/sdk';

import { sendManagedMessage } from './managed_mcp_session.ts';

const BETAS = ['managed-agents-test'];
const receipt = {
  id: 'receipt',
  type: 'user.message',
  processed_at: '2026-09-08T00:00:00Z',
};
const idle = {
  id: 'idle',
  type: 'session.status_idle',
  processed_at: '2026-09-08T00:00:01Z',
};

test('managed MCP send retries only receipt-scoped transient reads', async () => {
  // Cause/effect decision table:
  // | Rule | Event send | history read | effect |
  // | S1 | returns exact receipt | one classified 503 then stable | one send,
  // |    |                       |                                | two reads,
  // |    |                       |                                | terminal |
  // | S2 | returns exact receipt | unclassified 400              | one send,
  // |    |                       |                                | one read,
  // |    |                       |                                | same error |
  // Constraints: the Event write has no stable idempotency key and therefore
  // stays outside the retry closure; only events.list is replay-safe. Persistent
  // classified failures and deadlines are owned by the shared harness W9 rule.
  for (const rule of ['S1', 'S2']) {
    let sends = 0;
    let reads = 0;
    const readError = rule === 'S1'
      ? Object.assign(new Error('projection sources changed'), { status: 503 })
      : Object.assign(new Error('invalid history request'), { status: 400 });
    const client = {
      beta: {
        sessions: {
          events: {
            send: async () => {
              sends += 1;
              return { data: [receipt] };
            },
            list: async function* list() {
              reads += 1;
              if (rule === 'S2' || reads === 1) throw readError;
              yield receipt;
              yield idle;
            },
          },
        },
      },
    } as unknown as Anthropic;

    if (rule === 'S1') {
      const events = await sendManagedMessage(client, 'session', 'add 2 5', BETAS);
      assert.deepEqual(events, [receipt, idle], 'S1 returns the stable committed history');
      assert.equal(reads, 2, 'S1 retries only the failed history read');
    } else {
      await assert.rejects(
        sendManagedMessage(client, 'session', 'add 2 5', BETAS),
        (error) => error === readError,
      );
      assert.equal(reads, 1, 'S2 surfaces an unclassified read without retry');
    }
    assert.equal(sends, 1, `${rule} never replays the unkeyed Event write`);
  }
});
