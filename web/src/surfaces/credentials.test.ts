import { describe, expect, it } from "vitest";
import type { CredentialSource } from "../lib/api/types";
import { credentialSourceDisplayName } from "./credentials";

function source(overrides: Partial<CredentialSource>): CredentialSource {
  return {
    id: "cred:opaque:internal:id",
    workspace_id: "workspace_local",
    kind: "vault",
    status: "active",
    version: 1,
    ...overrides,
  } as CredentialSource;
}

describe("credentialSourceDisplayName", () => {
  it("uses a readable provider name instead of the opaque source id", () => {
    expect(credentialSourceDisplayName(source({ provider_id: "anthropic" }), "en"))
      .toBe("Anthropic model credential");
  });

  it("describes worker and environment sources without parsing their ids", () => {
    expect(credentialSourceDisplayName(source({ kind: "worker_local" }), "en"))
      .toBe("Worker-provided credential");
    expect(credentialSourceDisplayName(source({ env_key: "MODEL_TOKEN" }), "zh"))
      .toBe("MODEL_TOKEN 运行凭证");
  });
});
