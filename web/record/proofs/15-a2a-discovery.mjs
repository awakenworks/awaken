// A2A proof: inspect the genuine public Agent Card served by this deployment.
import { BACKEND, requireOk } from "../support/control-plane.mjs";

export async function run({ page, goto, checkpoint, expect, click }) {
  await goto("/w/default/a2a-servers");
  const inbound = page.locator(".card").filter({ hasText: /deployment is an A2A Agent|部署是 A2A Agent/ });
  await expect(inbound).toBeVisible();
  await checkpoint("the production server publishes a valid well-known Agent Card", async () => {
    const response = await page.request.get(`${BACKEND}/.well-known/agent-card.json`);
    await requireOk(response, "local A2A Agent Card");
    const localCard = await response.json();
    expect(JSON.stringify(localCard)).toMatch(/protocolVersion|supportedInterfaces|capabilities/);
    await expect(inbound).toContainText("/v1/a2a/message:send");
  });
  await click(page.getByRole("button", { name: /Inspect published card|检查已发布 Card/ }));
  const card = page.locator("pre");
  await checkpoint("the console renders the same live card published to A2A peers", async () => {
    await expect(card).toContainText(/protocolVersion|protocol_version|supportedInterfaces/);
    const response = await page.request.get(`${BACKEND}/.well-known/agent-card.json`);
    await requireOk(response, "rendered local A2A Agent Card");
    const published = JSON.stringify(await response.json(), null, 2);
    expect((await card.innerText()).trim()).toBe(published);
  });
}
