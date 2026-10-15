import assert from "node:assert/strict";
import fs from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import test from "node:test";
import OwnedDataReporter from "../e2e/owned-data-reporter.mjs";

const ownedRoot = () => fs.mkdtempSync(join(tmpdir(), "awaken-console-e2e."));

// Ownership x filesystem outcome x original verdict: only an owned real
// directory may be removed; cleanup failure rejects success, never masks an
// earlier failed/timed-out/interrupted run. Repeated cleanup is idempotent.
test("owned data cleanup is complete and idempotent", () => {
  const directory = ownedRoot();
  fs.writeFileSync(join(directory, "state.db"), "owned test data");
  const reporter = new OwnedDataReporter({ directory });
  assert.equal(reporter.onEnd({ status: "passed" }), undefined);
  assert.equal(fs.existsSync(directory), false);
  assert.equal(reporter.onEnd({ status: "passed" }), undefined);
});

test("foreign directories are retained and reject a passing run", (t) => {
  t.mock.method(console, "error", () => {});
  const foreign = fs.mkdtempSync(join(tmpdir(), "awaken-browser-foreign."));
  try {
    fs.writeFileSync(join(foreign, "retain"), "foreign data");
    for (const directory of [undefined, tmpdir(), foreign]) {
      assert.deepEqual(new OwnedDataReporter({ directory }).onEnd({ status: "passed" }), { status: "failed" });
    }
    assert.equal(fs.readFileSync(join(foreign, "retain"), "utf8"), "foreign data");
  } finally {
    fs.rmSync(foreign, { recursive: true, force: true });
  }
});

test("linked directories cannot grant cleanup authority", (t) => {
  t.mock.method(console, "error", () => {});
  const target = fs.mkdtempSync(join(tmpdir(), "awaken-browser-link-target."));
  const directory = ownedRoot();
  fs.rmdirSync(directory);
  fs.symlinkSync(target, directory, process.platform === "win32" ? "junction" : "dir");
  try {
    fs.writeFileSync(join(target, "retain"), "target data");
    assert.deepEqual(new OwnedDataReporter({ directory }).onEnd({ status: "passed" }), { status: "failed" });
    assert.equal(fs.readFileSync(join(target, "retain"), "utf8"), "target data");
  } finally {
    fs.unlinkSync(directory);
    fs.rmSync(target, { recursive: true, force: true });
  }
});

test("cleanup IO failure fails success and preserves every earlier verdict", (t) => {
  t.mock.method(console, "error", () => {});
  const directory = ownedRoot();
  const failure = t.mock.method(fs, "rmSync", () => { throw Object.assign(new Error("busy fixture"), { code: "EBUSY" }); });
  const reporter = new OwnedDataReporter({ directory });
  try {
    for (const status of ["passed", "failed", "timedout", "interrupted"]) {
      assert.deepEqual(reporter.onEnd({ status }), { status: status === "passed" ? "failed" : status });
    }
    assert.equal(fs.existsSync(directory), true);
  } finally {
    failure.mock.restore();
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test("real Playwright finalizes owned SQLite data after webServer exits on pass and failure", async () => {
  for (const passes of [true, false]) {
    const directory = ownedRoot();
    const scratch = fs.mkdtempSync(join(tmpdir(), "awaken-browser-runner."));
    const socket = createServer();
    await new Promise((resolve) => socket.listen(0, "127.0.0.1", resolve));
    const port = socket.address().port;
    await new Promise((resolve) => socket.close(resolve));
    const token = randomUUID();
    const origin = `http://127.0.0.1:${port}`;
    const reporterUrl = new URL("../e2e/owned-data-reporter.mjs", import.meta.url).href;
    const testUrl = new URL("../node_modules/@playwright/test/index.mjs", import.meta.url).href;
    const cli = fileURLToPath(new URL("../node_modules/@playwright/test/cli.js", import.meta.url));
    fs.writeFileSync(join(scratch, "server.mjs"), `
      import { DatabaseSync } from 'node:sqlite';
      import { createServer } from 'node:http';
      import { writeFileSync } from 'node:fs';
      const db = new DatabaseSync(${JSON.stringify(join(directory, "admin.db"))});
      db.exec('CREATE TABLE fixture (id INTEGER)');
      writeFileSync('server.pid', String(process.pid));
      const server = createServer((request, response) => {
        response.end(${JSON.stringify(token)});
        if (request.url === ${JSON.stringify(`/shutdown/${token}`)}) { db.close(); server.close(); }
      });
      server.listen(${port}, '127.0.0.1');
    `);
    fs.writeFileSync(join(scratch, "reporter.mjs"), `
      import Cleanup from ${JSON.stringify(reporterUrl)};
      import { readFileSync } from 'node:fs';
      export default class extends Cleanup {
        onEnd(result) {
          const pid = Number(readFileSync('server.pid', 'utf8'));
          try { process.kill(pid, 0); return { status: 'failed' }; }
          catch (error) { if (error.code !== 'ESRCH') throw error; }
          return super.onEnd(result);
        }
      }
    `);
    fs.writeFileSync(join(scratch, "ok.spec.mjs"), `import {test, expect} from ${JSON.stringify(testUrl)}; test('fixture', () => expect(true).toBe(${passes}));`);
    fs.writeFileSync(join(scratch, "playwright.config.mjs"), `export default ${JSON.stringify({
      testDir: scratch, testMatch: "ok.spec.mjs", outputDir: join(scratch, "results"),
      reporter: [["./reporter.mjs", { directory }], ["json", { outputFile: join(scratch, "report.json") }]],
      webServer: { command: "node server.mjs", cwd: scratch, url: origin, timeout: 10000, reuseExistingServer: false },
    })};`);
    try {
      const result = spawnSync(process.execPath, [cli, "test", "--config", join(scratch, "playwright.config.mjs"), "--workers=1"], {
        cwd: scratch, env: process.env, encoding: "utf8", timeout: 30000,
      });
      assert.equal(result.error, undefined, result.error?.message);
      assert.equal(result.status, passes ? 0 : 1, result.stdout + result.stderr);
      const receipt = JSON.parse(fs.readFileSync(join(scratch, "report.json"), "utf8"));
      assert.deepEqual(receipt.errors, [], "startup and runner errors cannot stand in for the intended test failure");
      assert.equal(receipt.stats.expected, passes ? 1 : 0);
      assert.equal(receipt.stats.unexpected, passes ? 0 : 1);
      assert.equal(receipt.stats.skipped, 0);
      const specifications = receipt.suites.flatMap((suite) => suite.specs);
      assert.equal(specifications.length, 1, "the intended fixture must actually run");
      assert.equal(specifications[0].title, "fixture");
      assert.equal(specifications[0].tests[0].results[0].status, passes ? "passed" : "failed");
      assert.equal(fs.existsSync(directory), false, "cleanup follows actual backend termination even when a test fails");
    } finally {
      // Only our nonce-bearing fixture can authorize this fallback after a
      // child timeout. Its SQLite handle closes before the response completes.
      try {
        const identity = await fetch(origin, { signal: AbortSignal.timeout(2000) });
        if (await identity.text() === token) await fetch(`${origin}/shutdown/${token}`, { signal: AbortSignal.timeout(2000) });
      } catch { /* Playwright already stopped the owned server. */ }
      fs.rmSync(directory, { recursive: true, force: true });
      fs.rmSync(scratch, { recursive: true, force: true });
    }
  }
});
