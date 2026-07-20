// A2A proof: discover a genuinely configured remote delegate through its Agent Card.

const DELEGATE_ID = process.env.A2A_DELEGATE_ID ?? "";

export const story = {
  promise: "Discover a remote A2A Agent before delegation and inspect the capability contract the peer actually publishes.",
  effect: "The A2A page fetches the configured remote Agent Card and renders its protocol version, interface, and capabilities.",
  aha: "Remote Agents enter through a discoverable contract—the platform fails closed instead of guessing what a peer can do.",
  loyalty: "Standards-based discovery preserves interoperability as remote Agent providers and implementations change.",
  satisfaction: "A visible Agent Card makes connection diagnosis immediate and prevents blind delegation attempts.",
  advocacy: "A real remote card appearing in the console is concrete proof of cross-platform Agent interoperability.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  if (!DELEGATE_ID) throw new Error("15-a2a-discovery requires a host with A2A_DELEGATE_ID registered as a real remote delegate");
  await goto("/w/default/a2a-servers");
  await intro(
    "Verify a remote Agent's identity and capabilities before trusting it with delegated work.",
    "Route discovery through the registered A2A delegate and render the peer's real Agent Card, failing closed when absent.",
  );
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
