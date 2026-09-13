import { withoutAmbientProviderKeys } from '../../e2e/process_environment.mjs';
import { spawnProduction } from '../../e2e/harness.mjs';

// Browser startup decision rule: an owned empty root must be explicitly
// initialized before Serve; initialization failure must prevent readiness.
// Reuse the shared production fixture's I1-I5 rules and typed local/no-login
// deployment. Playwright readiness and the browser assertions exercise success;
// the installation harness tests own initialization failure and restart cases.
const dataDir = process.env.AWAKEN_E2E_OWNED_DATA_DIR;
if (!dataDir) throw new Error('browser backend requires its owned data directory');
if (process.env.AWAKEN_E2E_CARGO_TARGET_DIR) {
  process.env.CARGO_TARGET_DIR = process.env.AWAKEN_E2E_CARGO_TARGET_DIR;
}
// L1 live browser owns a provider key -> only config ingress may receive it;
// L2 production child -> canonical environment filter removes ambient keys.
// Live MCP browser acceptance proves setup+execution without bypassing this gate.
const server = spawnProduction(dataDir, Number(process.env.AWAKEN_E2E_BACKEND_PORT), {}, withoutAmbientProviderKeys(process.env));
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => server.kill(signal));
}
server.on('error', (error) => { throw error; });
server.on('exit', (code) => { process.exitCode = code ?? 1; });
