import { spawn, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const dataDir = process.env.AWAKEN_E2E_OWNED_DATA_DIR;
const port = process.env.AWAKEN_E2E_BACKEND_PORT;
const cargoTargetDir = process.env.AWAKEN_E2E_CARGO_TARGET_DIR;
if (!dataDir || !port || !cargoTargetDir) {
  throw new Error("browser E2E backend requires its owned data directory, port, and Cargo target directory");
}
const childEnvironment = { ...process.env, CARGO_TARGET_DIR: cargoTargetDir };
const generatedConfig = join(dataDir, "browser-e2e.toml");
const baseConfig = readFileSync("web/e2e/browser-e2e.toml", "utf8");
writeFileSync(generatedConfig, `${baseConfig.trimEnd()}\ndata_dir = ${JSON.stringify(dataDir)}\n`);

const cargoArgs = ["run", "--release", "--quiet", "-p", "awaken-cli", "--bin", "awaken", "--"];
const commonArgs = ["--config", generatedConfig];
const initialized = spawnSync("cargo", [
  ...cargoArgs,
  "database", "migrate",
  ...commonArgs,
  "--initialize-installation",
  "--initialization-reference", "web-console-e2e",
], { cwd: process.cwd(), env: childEnvironment, stdio: "inherit" });
if (initialized.error) throw initialized.error;
if (initialized.status !== 0) process.exit(initialized.status ?? 1);

const backend = spawn("cargo", [
  ...cargoArgs,
  "all-in-one",
  ...commonArgs,
  "--port", port,
  "--no-browser",
  "--identity-mode", "no-login",
], { cwd: process.cwd(), env: childEnvironment, stdio: "inherit" });

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => backend.kill(signal));
}
backend.on("error", (error) => { throw error; });
backend.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});
