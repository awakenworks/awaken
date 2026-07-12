// Full-microservices driver — the TEST LOGIC half of `microservices_e2e.sh`.
// Submits ONE durable run through the brain's durable HTTP ingress and polls the
// committed thread history until the assistant reply carrying the remote-hand
// marker appears. That reply is only produced if the brain routed its `bash` tool
// call to the SEPARATE hand pod (Direct topology) and committed the result through
// Postgres — so a green poll proves the brain→hand→brain→Postgres round trip.
//
// The authoritative exactly-once / terminal-phase / marker-in-Postgres assertions
// are done by the shell against Postgres directly (a per-pod projection cannot be
// trusted); this driver proves the durable HTTP surface accepts + drives the run.
//
// Run (node 22 strips the types natively):
//   node e2e/k3d/microservices_driver.ts run http://127.0.0.1:PORT
//
// Exit 0 + "OK …" on success; exit 1 + "FAIL …" on any assertion miss.

type Role = 'User' | 'Assistant' | 'Tool' | string;
interface Message { role: Role; text?: string }
interface MessagesResponse { messages?: Message[] }
interface SubmitResponse { run_id?: string; queued?: boolean }

const THREAD = process.env.THREAD ?? 'micro-1';
const MARKER = process.env.MARKER ?? 'REMOTE-HAND-OK-9f31';

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

async function submitBackground(base: string, text: string): Promise<string> {
  const body = await fetchJson<SubmitResponse>(`${base}/v1/durable/threads/${THREAD}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (body.queued !== true || !body.run_id) throw new Error(`unexpected submit body: ${JSON.stringify(body)}`);
  return body.run_id;
}

/** Poll the durable thread until an assistant reply carries the remote-hand marker. */
async function waitForMarker(base: string, timeoutMs = 60_000): Promise<Message> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const m = await fetchJson<MessagesResponse>(`${base}/v1/durable/threads/${THREAD}/messages`);
    const hit = (m.messages ?? []).find((x) => x.role === 'Assistant' && (x.text ?? '').includes(MARKER));
    if (hit) return hit;
    if (Date.now() > deadline) throw new Error(`marker '${MARKER}' never committed: ${JSON.stringify(m.messages)}`);
    await sleep(250);
  }
}

async function main(): Promise<void> {
  const [cmd, base] = process.argv.slice(2);
  if (cmd !== 'run' || !base) throw new Error(`usage: microservices_driver.ts run <baseUrl>`);

  // Submit a durable run: the brain enqueues it on the shared Postgres dispatch
  // queue, its dispatch pool claims + drives it, and every tool call is routed to
  // the separate hand pod over TCP. The final assistant turn echoes what the hand
  // said and is committed to Postgres.
  const runId = await submitBackground(base, 'run the hand');
  const reply = await waitForMarker(base);
  console.log(`OK run=${runId} marker="${(reply.text ?? '').trim()}"`);
}

main().catch((err: unknown) => {
  console.log(`FAIL ${err instanceof Error ? err.message : String(err)}`);
  process.exit(1);
});
