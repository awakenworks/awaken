// Installation proof: a copied release binary starts from a clean directory and
// serves its own production Console and API from one all-in-one listener.
import { readFileSync } from "node:fs";
import { BACKEND } from "../support/control-plane.mjs";

const RECEIPT_PATH = process.env.AWAKEN_RECORD_STARTUP_RECEIPT ?? "";

function escapeHtml(value) {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, wait }) {
  if (!RECEIPT_PATH) throw new Error("07-all-in-one-startup must be launched through record/install-all-in-one.mjs");
  const receipt = readFileSync(RECEIPT_PATH, "utf8");
  if (!receipt.includes("Awaken is ready") || !receipt.includes(`Console   ${BACKEND}`)) {
    throw new Error("captured clean-directory startup receipt is incomplete or belongs to another listener");
  }

  await intro(
    "Amir has one hour to prepare an Agent evaluation. He cannot spend it wiring separate control-plane services.",
    "He starts one copied release binary in an empty directory, then gives the team its Console URL.",
  );
  await page.setContent(`<!doctype html><html><head><meta charset="utf-8"><style>
    html,body{margin:0;width:100%;height:100%;background:#08090d;color:#edf0f7;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
    body{display:grid;place-items:center;background:radial-gradient(circle at 50% 38%,#191d31 0,#0b0d14 42%,#08090d 72%)}
    .window{width:min(1320px,82vw);border:1px solid #30364b;border-radius:16px;background:rgba(12,14,21,.96);box-shadow:0 28px 90px #000b;overflow:hidden}
    .bar{display:flex;align-items:center;gap:9px;padding:14px 18px;border-bottom:1px solid #252a3a;background:#11141d}.dot{width:12px;height:12px;border-radius:50%}.r{background:#ff6b6b}.y{background:#ffd166}.g{background:#61d095}.title{margin-left:14px;color:#8993ad;font-size:14px}
    pre{margin:0;padding:38px 44px 44px;white-space:pre-wrap;font-size:20px;line-height:1.62}.command{color:#9cc2ff}.ok{color:#8ee3b0}
  </style></head><body><section class="window" aria-label="Captured all-in-one startup receipt"><div class="bar"><i class="dot r"></i><i class="dot y"></i><i class="dot g"></i><span class="title">clean-directory startup · real captured output</span></div><pre><span class="command">$ ./awaken all-in-one --config ./all-in-one.toml</span>\n\n<span class="ok">${escapeHtml(receipt.trim())}</span></pre></section></body></html>`);
  await say("This is the captured output from the copied binary, not a prepared development server.", 3600);
  await checkpoint("one clean-directory process reports the all-in-one Console listener", async () => {
    await expect(page.getByLabel("Captured all-in-one startup receipt")).toContainText("Awaken is ready");
    await expect(page.getByLabel("Captured all-in-one startup receipt")).toContainText(BACKEND);
    const ready = await fetch(`${BACKEND}/readyz`);
    expect(ready.status).toBe(200);
  });

  await say("Amir opens the exact URL printed by that process and hands it to the evaluation team.", 3600);
  await goto("/w/default/overview");
  await checkpoint("the same listener serves a usable embedded production Console", async () => {
    await expect(page.getByText(/Set up, run, and verify an Agent|Agent 配置/).first()).toBeVisible();
    const shell = await fetch(`${BACKEND}/`);
    expect(shell.headers.get("content-type") ?? "").toContain("text/html");
    expect(await shell.text()).toContain('id="root"');
  });
  await clearCaption();
  await wait(900);
  await clearCaption();
}
