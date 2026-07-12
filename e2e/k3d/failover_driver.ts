// Cross-node failover driver (ADR-0022 D6 / ADR-0019) — the TEST LOGIC half of
// `failover_e2e.sh`. The shell owns cluster/port-forward/pod-kill (infrastructure);
// this typed driver owns the durable HTTP drive and the assertions, so a failure is
// a precise message, not a swallowed `set -e` abort.
//
// Run (node 22 strips the types natively):
//   node e2e/k3d/failover_driver.ts submit   http://127.0.0.1:PORT
//   node e2e/k3d/failover_driver.ts continue http://127.0.0.1:PORT
//
// Exit 0 + "OK …" on success; exit 1 + "FAIL …" on any assertion miss.

type Role = 'User' | 'Assistant' | 'Tool' | string;
interface Message { role: Role; text?: string }
interface MessagesResponse { messages?: Message[] }
interface SubmitResponse { run_id?: string; queued?: boolean }

const THREAD = process.env.THREAD ?? 'failover-thread-1';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** fetch that retries transient connection errors (a port-forward warmup blip is
 *  not a scenario failure) and non-2xx, up to a deadline. */
async function fetchJson<T>(url: string, init?: RequestInit, timeoutMs = 15_000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let lastErr: unknown;
  for (;;) {
    try {
      const res = await fetch(url, init);
      if (!res.ok) throw new Error(`HTTP ${res.status}: ${await res.text()}`);
      return (await res.json()) as T;
    } catch (err) {
      lastErr = err;
      if (Date.now() > deadline) throw new Error(`${url}: ${lastErr instanceof Error ? lastErr.message : String(lastErr)}`);
      await sleep(200);
    }
  }
}

const assistantReplies = (m: MessagesResponse): Message[] =>
  (m.messages ?? []).filter((x) => x.role === 'Assistant' && (x.text ?? '').length > 0);

async function submitBackground(base: string, text: string): Promise<string> {
  const body = await fetchJson<SubmitResponse>(`${base}/v1/durable/threads/${THREAD}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (body.queued !== true || !body.run_id) throw new Error(`unexpected submit body: ${JSON.stringify(body)}`);
  return body.run_id;
}

/** Poll the durable thread until at least `min` assistant replies are committed. */
async function waitForAssistants(base: string, min: number, timeoutMs = 30_000): Promise<Message[]> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const m = await fetchJson<MessagesResponse>(`${base}/v1/durable/threads/${THREAD}/messages`);
    const a = assistantReplies(m);
    if (a.length >= min) return a;
    if (Date.now() > deadline) throw new Error(`only ${a.length}/${min} assistant replies: ${JSON.stringify(m.messages)}`);
    await sleep(150);
  }
}

async function main(): Promise<void> {
  const [cmd, base] = process.argv.slice(2);
  if (!base) throw new Error(`usage: failover_driver.ts <submit|continue> <baseUrl>`);

  if (cmd === 'submit') {
    // Submit a durable run on this node; the fleet drives it, committing to Postgres.
    const runId = await submitBackground(base, 'hello from A');
    await waitForAssistants(base, 1);
    console.log(`OK run=${runId}`);
    return;
  }

  if (cmd === 'continue') {
    // (3) history from node A's turn must be visible here (served from Postgres).
    const before = await fetchJson<MessagesResponse>(`${base}/v1/durable/threads/${THREAD}/messages`);
    if (assistantReplies(before).length < 1) {
      throw new Error(`this node does not see node A's history: ${JSON.stringify(before.messages)}`);
    }
    // (4) a second turn on this node must commit a second assistant reply — the
    //     thread continues on a different node after A was deleted.
    await submitBackground(base, 'hello from B');
    const a = await waitForAssistants(base, 2);
    console.log(`OK assistants=${a.length}`);
    return;
  }

  throw new Error(`unknown command '${cmd}' (expected submit|continue)`);
}

main().catch((err: unknown) => {
  console.log(`FAIL ${err instanceof Error ? err.message : String(err)}`);
  process.exit(1);
});
