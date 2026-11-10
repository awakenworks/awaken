// Sample-only CLI and bounded observation; Awaken remains the Session owner.
import { parseArgs } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';

export function readCommand() {
  const argv = process.argv.slice(2);
  if (argv[0] === '--') argv.shift(); // pnpm forwards its script separator.
  const { values, positionals } = parseArgs({
    args: argv,
    options: {
      session: { type: 'string' }, history: { type: 'boolean' },
      receipt: { type: 'string' }, approve: { type: 'string' }, deny: { type: 'string' },
      help: { type: 'boolean' },
    },
    allowPositionals: true,
  });
  if (values.help) {
    console.log('Usage: managed [--session ID] [message | --history | --receipt ID | --approve EVENT_ID | --deny EVENT_ID]');
    process.exit(0);
  }
  const observationOnly = values.history || values.receipt;
  if ((observationOnly || values.approve || values.deny) && !values.session) {
    throw new Error('--history, --receipt, --approve and --deny require --session');
  }
  if ([values.history, values.receipt, values.approve, values.deny].filter(Boolean).length > 1
      || ((observationOnly || values.approve || values.deny) && positionals.length)) {
    throw new Error('Choose one of a message, --history, --receipt, --approve or --deny');
  }
  const timeoutMs = Number(process.env.AWAKEN_TIMEOUT_MS ?? 120_000);
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new Error('AWAKEN_TIMEOUT_MS must be positive');
  return { values, positionals, timeoutMs };
}

export async function observe(client, sessionId, receiptId, scope, timeoutMs) {
  // Observe only facts after this exact accepted input. An older idle or reply
  // in a reused Session cannot complete this request. Read retries never resend.
  const deadline = Date.now() + timeoutMs;
  let complete = false;
  while (Date.now() < deadline) {
    let events;
    const readSignal = AbortSignal.timeout(Math.max(1, deadline - Date.now()));
    try {
      events = [];
      for await (const item of client.beta.sessions.events.list(sessionId, { ...scope, order: 'asc', limit: 100 }, { signal: readSignal })) events.push(item);
    } catch (error) {
      if (readSignal.aborted) break;
      if (![429, 502, 503, 504].includes(error.status)) throw error;
      await delay(200);
      continue;
    }
    const index = events.findIndex(item => item.id === receiptId && item.processed_at);
    const delta = index < 0 ? [] : events.slice(index + 1);
    const terminal = delta.findLast(item => item.type === 'session.status_idle'
      || item.type === 'session.status_terminated' || item.type === 'session.deleted');
    if (terminal) {
      for (const item of delta) if (item.type === 'agent.message') {
        for (const block of item.content ?? []) if (block.type === 'text') console.log(block.text);
      }
      const reason = terminal.stop_reason?.type;
      if (terminal.type !== 'session.status_idle' || !reason || !['end_turn', 'requires_action', 'budget_reached'].includes(reason)) {
        const error = delta.findLast(item => item.type === 'session.error');
        throw new Error(`Session ended without success: ${JSON.stringify(error?.error ?? terminal)}`);
      }
      console.log(`status=${reason}`);
      if (reason !== 'end_turn') {
        console.log(`Waiting: ${JSON.stringify(terminal.stop_reason)}. Inspect the Session before continuing.`);
        process.exitCode = 2;
      }
      complete = true;
      break;
    }
    await delay(200);
  }
  if (!complete) throw new Error(`Observation timed out. Read the same Session with --session ${sessionId} --receipt ${receiptId}; no input was resent.`);
}
