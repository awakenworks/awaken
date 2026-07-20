// A2A proof: discover a genuinely configured remote delegate through its Agent Card.

const DELEGATE_ID = process.env.A2A_DELEGATE_ID ?? "";

export const story = {
  promise: "Publish this runtime as an A2A Agent and discover a remote peer before delegation using the same standard contract.",
  effect: "Awaken serves its own well-known Agent Card, then fetches and renders the configured remote peer's real card.",
  aha: "A2A is symmetric: Awaken is discoverable to peers and discovers them without guessing either side's capabilities.",
  loyalty: "Standards-based discovery preserves interoperability as remote Agent providers and implementations change.",
  satisfaction: "A visible Agent Card makes connection diagnosis immediate and prevents blind delegation attempts.",
  advocacy: "A real remote card appearing in the console is concrete proof of cross-platform Agent interoperability.",
};

export async function run({ page, goto, intro, say, beat, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  if (!DELEGATE_ID) throw new Error("15-a2a-discovery requires a host with A2A_DELEGATE_ID registered as a real remote delegate");
  await goto("/w/default/a2a-servers");
  await intro(
    "Verify a remote Agent's identity and capabilities before trusting it with delegated work.",
    "Serve a well-known Agent Card for inbound peers, then fetch the registered remote delegate's real card, failing closed when absent.",
  );
  const inbound = page.locator(".card").filter({ hasText: /already an A2A agent|已是 A2A Agent/ });
  await beat("Awaken already publishes the standard discovery card and message endpoints for inbound A2A clients.", inbound, 3600);
  await checkpoint("the production server publishes a valid well-known Agent Card", async () => {
    const response = await page.request.get("http://127.0.0.1:38080/.well-known/agent-card.json");
    expect(response.ok()).toBeTruthy();
    const localCard = await response.json();
    expect(JSON.stringify(localCard)).toMatch(/protocolVersion|supportedInterfaces|capabilities/);
    await expect(inbound).toContainText("/v1/a2a/message:send");
  });
  await type(page.getByPlaceholder("agent id"), DELEGATE_ID);
  await say("Fetch crosses the real outbound A2A boundary; no local placeholder card is synthesized.", 3800);
  await click(page.getByRole("button", { name: /Fetch card|获取/ }));
  const card = page.locator("pre");
  await checkpoint("the configured remote delegate returns a visible Agent Card", async () => {
    await expect(card).toContainText(DELEGATE_ID, { timeout: 15_000 });
    await expect(card).toContainText(/protocolVersion|protocol_version|supportedInterfaces/);
    const response = await page.request.get(`http://127.0.0.1:38080/v1/delegates/${DELEGATE_ID}/card`);
    expect(response.ok()).toBeTruthy();
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
