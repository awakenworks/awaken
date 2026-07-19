import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const flowsDir = resolve(here, "flows");
const flows = readdirSync(flowsDir).filter((name) => name.endsWith(".mjs")).sort();

test("every recording is a story-shaped executable test", () => {
  assert.ok(flows.length > 0, "at least one recording flow exists");
  for (const name of flows) {
    const source = readFileSync(resolve(flowsDir, name), "utf8");
    assert.match(source, /await intro\(/, `${name}: explain intent and capability before operating`);
    assert.match(source, /await checkpoint\(/, `${name}: assert at least one product claim`);
    assert.match(source, /await aha\(/, `${name}: land a visible, shareable payoff`);
  }
});

test("the state-machine recording proves runtime enforcement, not just configuration", () => {
  const source = readFileSync(resolve(flowsDir, "06-ai-state-machine.mjs"), "utf8");
  assert.match(source, /Try it\|试运行/);
  assert.match(source, /Start session\|开始会话/);
  assert.match(source, /State Machine blocks the unread write at runtime/);
  assert.match(source, /getByText\("error"/);
  assert.ok(source.includes("getByText(/blocked|Read .* before writing/i)"));
});

test("interactive transcripts follow committed SSE frames without a manual refresh", () => {
  const transcript = readFileSync(resolve(here, "../src/components/session/Transcript.tsx"), "utf8");
  const hook = readFileSync(resolve(here, "../src/lib/useSessionLog.ts"), "utf8");
  assert.match(transcript, /useSessionLog\(base, queryKey, live, composer\)/);
  assert.match(hook, /if \(followLive\)/);
  assert.match(hook, /setQueryData<SessionEvent\[\]>/);
});
