import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { PRODUCT_PROOFS } from "./catalog.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const requested = process.argv.slice(2).filter((argument) => argument !== "--");
const unknown = requested.filter((slug) => !PRODUCT_PROOFS.includes(slug));
if (unknown.length > 0) {
  throw new Error(`unknown product proof: ${unknown.join(", ")}`);
}
const proofs = requested.length > 0 ? requested : PRODUCT_PROOFS;
let failed = false;
for (const slug of proofs) {
  const script = slug === "07-all-in-one-startup" ? "install-all-in-one.mjs" : "proof-harness.mjs";
  const args = slug === "07-all-in-one-startup" ? [resolve(here, script)] : [resolve(here, script), slug];
  const result = spawnSync(process.execPath, args, {
    cwd: resolve(here, "../.."),
    env: process.env,
    stdio: "inherit",
    timeout: 210_000,
    killSignal: "SIGKILL",
  });
  if (result.status !== 0) failed = true;
}
process.exit(failed ? 1 : 0);
