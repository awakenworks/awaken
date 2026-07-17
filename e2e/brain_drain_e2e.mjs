// Brain admin surface (ADR-0022 D7): the connection-count metric + graceful drain,
// end to end over the real server binary. Autoscaling scrapes active_streams;
// scale-in flips /readyz to 503 so the Service stops routing before SIGTERM.
//
// Run: (from e2e/)  node brain_drain_e2e.mjs

import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38799);
const BASE = `http://127.0.0.1:${PORT}`;

async function main() {
  const { server } = spawnServer('echo', PORT, {});
  try {
    await waitForPort(PORT);

    // Ready and the metric surface is up.
    let r = await fetch(`${BASE}/readyz`);
    if (r.status !== 200) throw new Error(`/readyz before drain: ${r.status}`);
    pass('the Brain reports ready before draining');

    let m = await (await fetch(`${BASE}/metrics`)).text();
    // OTel appends scope labels: `name{otel_scope_name="awaken-brain"} <value>`.
    if (!/awaken_brain_active_streams(\{[^}]*\})? \d+/.test(m) || !/awaken_brain_draining(\{[^}]*\})? 0/.test(m)) {
      throw new Error(`/metrics missing gauges: ${m}`);
    }
    pass('the connection-count metric is exposed for autoscaling (draining=0)');

    // Drain: readiness flips to 503 so the Service/gateway stops routing new work.
    r = await fetch(`${BASE}/admin/drain`, { method: 'POST' });
    if (r.status !== 200) throw new Error(`/admin/drain: ${r.status}`);
    r = await fetch(`${BASE}/readyz`);
    if (r.status !== 503) throw new Error(`/readyz after drain should be 503, got ${r.status}`);
    pass('POST /admin/drain flips /readyz to 503 for graceful scale-in');

    m = await (await fetch(`${BASE}/metrics`)).text();
    if (!/awaken_brain_draining(\{[^}]*\})? 1/.test(m)) throw new Error(`draining gauge not set: ${m}`);
    pass('the draining gauge reflects the drain state');
  } finally {
    await stopServer(server);
  }
  console.log('\nBRAIN DRAIN E2E PASS: connection-count metric exposed; /admin/drain flips readiness to 503 for graceful scale-in.');
}

main().catch((err) => {
  console.error(`\nBRAIN DRAIN E2E FAIL: ${err.stack ?? err}`);
  process.exit(1);
});
