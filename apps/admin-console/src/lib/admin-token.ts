import { ADMIN_TOKEN_STORAGE_KEY } from "./config-api";

export interface TokenStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

function defaultStorage(): TokenStorage | null {
  if (typeof globalThis.localStorage === "undefined") {
    return null;
  }
  return globalThis.localStorage;
}

export function readAdminToken(storage: TokenStorage | null = defaultStorage()): string | null {
  if (!storage) return null;
  const stored = storage.getItem(ADMIN_TOKEN_STORAGE_KEY);
  if (!stored) return null;
  const trimmed = stored.trim();
  return trimmed.length > 0 ? trimmed : null;
}

export function writeAdminToken(
  token: string,
  storage: TokenStorage | null = defaultStorage(),
): void {
  if (!storage) return;
  storage.setItem(ADMIN_TOKEN_STORAGE_KEY, token.trim());
}

export function clearAdminToken(storage: TokenStorage | null = defaultStorage()): void {
  if (!storage) return;
  storage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
}

/// Mask a token for display: keep the first 4 and last 2 characters.
export function maskAdminToken(token: string | null | undefined): string {
  if (!token) return "—";
  const trimmed = token.trim();
  if (trimmed.length <= 8) {
    return "••••";
  }
  return `${trimmed.slice(0, 4)}…${trimmed.slice(-2)}`;
}
