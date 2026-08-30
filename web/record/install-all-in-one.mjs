// Run the installation proof from a genuinely clean directory. This owns the
// temporary all-in-one lifecycle; proof-harness.mjs owns browser assertions.
import { spawn } from "node:child_process";
import { chmodSync, copyFileSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
// Installation proof must exercise the same optimized artifact we ship. Using
// a debug binary here would make the recording slower and would not validate
// the release packaging path requested by operators.
const sourceBinary = resolve(repo, "target/release/awaken");
const root = mkdtempSync(join(tmpdir(), "awaken-install-recording-"));
const binary = join(root, "awaken");
const config = join(root, "all-in-one.toml");
const receiptPath = join(root, "startup-receipt.txt");
const port = Number(process.env.AWAKEN_RECORD_INSTALL_PORT ?? 38082);
const origin = `http://127.0.0.1:${port}`;

// This chapter proves a clean installation boundary. The parent recording
// process may legitimately hold Provider credentials for other live chapters,
// but forwarding them into a freshly launched Awaken process both weakens the
// boundary and triggers Awaken's ambient-secret fail-closed check. Provider
// credentials must enter through provider-connections after startup.
const serviceEnvironment = { ...process.env };
for (const name of Object.keys(serviceEnvironment)) {
  if (/(?:^|_)(?:API_KEY|ACCESS_TOKEN)$/i.test(name) || /^(?:GOOGLE_APPLICATION_CREDENTIALS|AZURE_OPENAI_API_KEY)$/i.test(name)) {
    delete serviceEnvironment[name];
  }
}

copyFileSync(sourceBinary, binary);
chmodSync(binary, 0o755);
writeFileSync(config, [
  'role = "all-in-one"',
  `bind = "127.0.0.1:${port}"`,
  `data_dir = ${JSON.stringify(join(root, "data"))}`,
  "no_browser = true",
  'identity_mode = "no-login"',
  'sandbox_tier = "local"',
  "acp_clis = []",
  "",
].join("\n"));

let output = "";
const host = spawn(binary, ["all-in-one", "--config", config], {
  cwd: root,
  env: serviceEnvironment,
  stdio: ["ignore", "pipe", "pipe"],
});
host.stdout.on("data", (chunk) => { output += chunk; });
host.stderr.on("data", (chunk) => { output += chunk; });

let harness;
try {
  await waitForReady(host, origin, () => output);
  const receipt = output.split("\n").filter((line) =>
    /Awaken is ready|Console\s+http|Data\s+|Stop\s+Ctrl\+C/.test(line)
  ).join("\n");
  writeFileSync(receiptPath, `${receipt}\n`);
  harness = spawn(process.execPath, [resolve(here, "proof-harness.mjs"), "07-all-in-one-startup"], {
    cwd: repo,
    env: {
      ...process.env,
      BACKEND_URL: origin,
      CONSOLE_URL: origin,
      AWAKEN_RECORD_STARTUP_RECEIPT: receiptPath,
      // This clean no-login instance owns its own authentication boundary.
      // Never send the parent all-in-one instance's one-time setup handoff to
      // a different origin: the token is intentionally origin-local.
      AWAKEN_RECORD_SETUP_TOKEN: "",
    },
    stdio: "inherit",
  });
  const code = await exitCode(harness);
  if (code !== 0) process.exitCode = code || 1;
} finally {
  if (harness && harness.exitCode == null) harness.kill("SIGINT");
  if (host.exitCode == null) {
    host.kill("SIGINT");
    await Promise.race([exitCode(host), new Promise((resolveWait) => setTimeout(resolveWait, 3000))]);
  }
  rmSync(root, { recursive: true, force: true });
}

async function waitForReady(processHandle, url, readOutput) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (processHandle.exitCode != null) throw new Error(`clean all-in-one exited early:\n${readOutput()}`);
    try {
      const response = await fetch(`${url}/readyz`, { signal: AbortSignal.timeout(700) });
      if (response.ok) return;
    } catch {
      // Startup owns this bounded retry window.
    }
    await new Promise((resolveWait) => setTimeout(resolveWait, 200));
  }
  throw new Error(`clean all-in-one did not become ready:\n${readOutput()}`);
}

function exitCode(processHandle) {
  if (processHandle.exitCode != null) return Promise.resolve(processHandle.exitCode);
  return new Promise((resolveExit) => processHandle.once("exit", (code) => resolveExit(code ?? 1)));
}
