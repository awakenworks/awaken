import { describe, expect, it } from "vitest";
import { extractAccessTokenFromFragment, extractOAuthErrorFromFragment } from "./auth";

describe("extractAccessTokenFromFragment", () => {
  it("extracts access_token from a well-formed fragment", () => {
    expect(extractAccessTokenFromFragment("#access_token=sess-abc123")).toBe("sess-abc123");
  });

  it("works without the leading #", () => {
    expect(extractAccessTokenFromFragment("access_token=tok")).toBe("tok");
  });

  it("returns null when access_token is absent", () => {
    expect(extractAccessTokenFromFragment("#oauth_error=denied")).toBeNull();
  });

  it("returns null for an empty fragment", () => {
    expect(extractAccessTokenFromFragment("")).toBeNull();
  });

  it("returns null for a blank token value", () => {
    expect(extractAccessTokenFromFragment("#access_token=   ")).toBeNull();
  });

  it("trims surrounding whitespace from the token", () => {
    expect(extractAccessTokenFromFragment("#access_token= tok ")).toBe("tok");
  });
});

describe("extractOAuthErrorFromFragment", () => {
  it("extracts oauth_error from a well-formed fragment", () => {
    expect(extractOAuthErrorFromFragment("#oauth_error=access_denied")).toBe("access_denied");
  });

  it("returns null when oauth_error is absent", () => {
    expect(extractOAuthErrorFromFragment("#access_token=tok")).toBeNull();
  });

  it("returns null for an empty fragment", () => {
    expect(extractOAuthErrorFromFragment("")).toBeNull();
  });
});
