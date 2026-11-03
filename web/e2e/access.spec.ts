import { expect, test } from "@playwright/test";
import { isApiRequest, workspaceId } from "./workspace";

test("service key form supports automatic principals and reports selected-principal refusal", async ({ page, request }) => {
  // C/E table: C1=principal blank/whitespace/named; C2=mint pending/refused.
  // R1 blank or whitespace + idle -> enabled button, POST omits principal_id;
  // R2 pending -> disabled button until that request settles; R3 named -> send
  // the trimmed principal, preserve the server's 403 and keep the form usable.
  // Controlled route responses prove UI behavior only. The real Control test
  // selected_service_principals_require_org_credential_management owns the PDP.
  const requests: Record<string, unknown>[] = [];
  let releaseFirst!: () => void;
  const firstResponse = new Promise<void>((resolve) => { releaseFirst = resolve; });
  const refusal = "Organization-level credential management is required.";
  const workspace = await workspaceId(request);
  await page.route("**/v1/**", async (route) => {
    const request = route.request();
    if (isApiRequest(request.url(), "/v1/config/capabilities")) {
      return route.fulfill({ json: {
        identity: { mode: "self-managed", cloud_login_enabled: false, authenticated: true },
        models: { local_catalog_enabled: true, byok_enabled: true, cloud_models_enabled: false, profile_authoring_enabled: false },
        surfaces: { managed_runtime: true, access_management: true },
      } });
    }
    if (!isApiRequest(request.url(), "/v1/config/iam/tokens")) return route.fallback();
    if (request.method() === "GET") return route.fulfill({ json: [] });
    if (request.method() !== "POST") return route.fallback();
    const body = request.postDataJSON() as Record<string, unknown>;
    requests.push(body);
    if (requests.length === 1) await firstResponse;
    if (body.principal_id) {
      return route.fulfill({ status: 403, json: {
        type: "error", error: { type: "permission_error", message: refusal },
      } });
    }
    return route.fulfill({ status: 201, json: {
      token: "inert-one-time-ui-fixture", // awaken-allow(secret): -- inert UI response fixture, never an authentication credential
      api_token: { id: `ui-token-${requests.length}` },
    } });
  });

  try {
    await page.goto("/w/default/access");
    const principal = page.getByLabel("Service principal · optional", { exact: true });
    const create = page.getByRole("button", { name: "Create key", exact: true });
    await expect(principal).toHaveValue("");
    await expect(create).toBeEnabled();
    await create.click();
    await expect.poll(() => requests.length).toBe(1);
    expect(requests[0]).not.toHaveProperty("principal_id");
    expect(requests[0]).toHaveProperty("workspace_id", workspace);
    await expect(create).toBeDisabled();
    releaseFirst();
    await expect(create).toBeEnabled();

    await principal.fill("   ");
    await create.click();
    await expect.poll(() => requests.length).toBe(2);
    expect(requests[1]).not.toHaveProperty("principal_id");
    await expect(create).toBeEnabled();

    await principal.fill("  selected-ui-fixture  ");
    await create.click();
    await expect.poll(() => requests.length).toBe(3);
    expect(requests[2]).toHaveProperty("principal_id", "selected-ui-fixture");
    await expect(page.locator(".err").filter({ hasText: refusal }).first()).toBeVisible();
    await expect(create).toBeEnabled();
  } finally {
    releaseFirst();
  }
});
