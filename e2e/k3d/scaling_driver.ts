// Horizontal-scaling driver (ADR-0019) — the TEST LOGIC half of `scaling_e2e.sh`.
// Fires M concurrent durable submissions across the brain fleet (through the
// load-balanced Service). Consistency is asserted authoritatively against Postgres
// by the shell (the per-pod message projection is a cache and cannot be trusted for
// a fleet); this driver only proves every submission was accepted and enqueued.
//
// Run (node 22 strips the types natively):
//   node e2e/k3d/scaling_driver.ts submit http://127.0.0.1:PORT 20
//
// Exit 0 + "OK submitted=<M>" when all M enqueue; exit 1 + "FAIL …" otherwise.

interface SubmitResponse { run_id?: string; queued?: boolean }

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

async function submitOnce(base: string, thread: string, text: string): Promise<string> {
  const deadline = Date.now() + 15_000;
  let lastErr: unknown;
  for (;;) {
    try {
      const res = await fetch(`${base}/v1/durable/threads/${thread}/submit_background`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}: ${await res.text()}`);
      const body = (await res.json()) as SubmitResponse;
      if (body.queued !== true || !body.run_id) throw new Error(`unexpected body: ${JSON.stringify(body)}`);
      return body.run_id;
    } catch (err) {
      lastErr = err;
      if (Date.now() > deadline) throw new Error(`${thread}: ${lastErr instanceof Error ? lastErr.message : String(lastErr)}`);
      await sleep(200);
    }
  }
}

async function main(): Promise<void> {
  const [cmd, base, mArg] = process.argv.slice(2);
  const M = Number(mArg ?? '20');
  if (cmd !== 'submit' || !base || !Number.isFinite(M) || M <= 0) {
    throw new Error(`usage: scaling_driver.ts submit <baseUrl> <M>`);
  }
  const prefix = process.env.THREAD_PREFIX ?? 'scale';
  // Fire all M at once so the fleet claims them concurrently (the SKIP-LOCKED race).
  const runIds = await Promise.all(
    Array.from({ length: M }, (_, i) => submitOnce(base, `${prefix}-${i}`, `hello ${i}`)),
  );
  const unique = new Set(runIds);
  if (unique.size !== M) throw new Error(`expected ${M} distinct run ids, got ${unique.size}`);
  console.log(`OK submitted=${M}`);
}

main().catch((err: unknown) => {
  console.log(`FAIL ${err instanceof Error ? err.message : String(err)}`);
  process.exit(1);
});
