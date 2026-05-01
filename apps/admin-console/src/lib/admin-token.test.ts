import { beforeEach, describe, expect, it } from "vitest";
import {
  clearAdminToken,
  maskAdminToken,
  readAdminToken,
  writeAdminToken,
  type TokenStorage,
} from "./admin-token";
import { ADMIN_TOKEN_STORAGE_KEY } from "./config-api";

class MemoryStorage implements TokenStorage {
  private store = new Map<string, string>();

  getItem(key: string): string | null {
    return this.store.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.store.set(key, value);
  }

  removeItem(key: string): void {
    this.store.delete(key);
  }
}

let storage: MemoryStorage;

beforeEach(() => {
  storage = new MemoryStorage();
});

describe("readAdminToken", () => {
  it("returns null when nothing is stored", () => {
    expect(readAdminToken(storage)).toBeNull();
  });

  it("trims whitespace and returns the token", () => {
    storage.setItem(ADMIN_TOKEN_STORAGE_KEY, "  secret  ");
    expect(readAdminToken(storage)).toBe("secret");
  });

  it("treats whitespace-only values as missing", () => {
    storage.setItem(ADMIN_TOKEN_STORAGE_KEY, "   ");
    expect(readAdminToken(storage)).toBeNull();
  });

  it("returns null when storage is unavailable", () => {
    expect(readAdminToken(null)).toBeNull();
  });
});

describe("writeAdminToken", () => {
  it("writes the trimmed token to storage", () => {
    writeAdminToken("  abc  ", storage);
    expect(storage.getItem(ADMIN_TOKEN_STORAGE_KEY)).toBe("abc");
  });

  it("is a no-op when storage is unavailable", () => {
    expect(() => writeAdminToken("abc", null)).not.toThrow();
  });
});

describe("clearAdminToken", () => {
  it("removes the stored token", () => {
    storage.setItem(ADMIN_TOKEN_STORAGE_KEY, "abc");
    clearAdminToken(storage);
    expect(storage.getItem(ADMIN_TOKEN_STORAGE_KEY)).toBeNull();
  });
});

describe("maskAdminToken", () => {
  it("returns a placeholder when the token is missing", () => {
    expect(maskAdminToken(null)).toBe("—");
    expect(maskAdminToken("")).toBe("—");
  });

  it("masks short tokens entirely", () => {
    expect(maskAdminToken("short")).toBe("••••");
    expect(maskAdminToken("12345678")).toBe("••••");
  });

  it("shows the first four and last two characters of long tokens", () => {
    expect(maskAdminToken("abcdef0123456789")).toBe("abcd…89");
  });
});
