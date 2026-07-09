// k6 load test for the durable dispatch pool (O2/O4, scaling capstone / C10).
//
// Concurrent virtual users submit background runs to distinct threads; each run
// must be driven to completion by the process pool over the ONE shared durable
// queue and commit an assistant reply. The `checks: rate==1.0` threshold makes
// k6 FAIL if a single run is lost under load — the zero-loss guarantee under
// concurrency. Throughput/latency are reported by k6.
//
// Driven by e2e/k6/run_durable_load.sh, which starts the durable server first.
import http from 'k6/http';
import { check, sleep } from 'k6';

const BASE = __ENV.BASE_URL || 'http://127.0.0.1:38791';
const VUS = Number(__ENV.VUS || 20);
const ITERS = Number(__ENV.ITERS || 10);

export const options = {
  scenarios: {
    submit_and_drain: {
      executor: 'per-vu-iterations',
      vus: VUS,
      iterations: ITERS,
      maxDuration: '120s',
    },
  },
  thresholds: {
    // Zero loss: every submitted run must complete. Any miss fails the test.
    checks: ['rate==1.0'],
    http_req_failed: ['rate==0.0'],
  },
};

export default function () {
  const thread = `load-${__VU}-${__ITER}`;

  const res = http.post(
    `${BASE}/v1/durable/threads/${thread}/submit_background`,
    JSON.stringify({ text: 'load' }),
    { headers: { 'content-type': 'application/json' } },
  );
  check(res, { 'submit accepted (200)': (r) => r.status === 200 });

  // Poll committed truth until the pool drove this run and committed a reply —
  // the per-run proof that no submission was dropped under concurrent load.
  let completed = false;
  for (let i = 0; i < 150; i++) {
    const m = http.get(`${BASE}/v1/durable/threads/${thread}/messages`);
    if (m.status === 200) {
      const msgs = m.json('messages') || [];
      if (msgs.some((x) => x.role === 'Assistant' && (x.text || '').length > 0)) {
        completed = true;
        break;
      }
    }
    sleep(0.1);
  }
  check(completed, { 'run driven to completion (no loss)': (c) => c === true });
}
