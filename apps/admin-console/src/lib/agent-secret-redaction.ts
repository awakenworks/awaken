import type { AgentSpec, RemoteEndpoint } from "./api/types";
import { deepEqualCanonical } from "./agent-editor-canonical";

/** Exact-match buckets for short / ambiguous secret names where substring
 *  matching would over-redact. Standalone `token` is a secret;
 *  `max_context_tokens` contains "token" but is not one.
 *  Keys are normalized via `normalizeSecretKey` before lookup. */
const EXACT_SECRET_KEYS = new Set(["token", "apikey", "xapikey"]);

/** Substring patterns specific enough to be safe; `token` is intentionally
 *  excluded. Compound `*_token` shapes live below as their full form. */
const SENSITIVE_AUTH_KEY_PATTERNS = [
  "secret",
  "password",
  "passphrase",
  "authorization",
  "credential",
  "privatekey",
  "clientsecret",
  "cookie",
  "jwt",
  "bearer",
  "session",
  "accesskey",
  "accesstoken",
  "refreshtoken",
  "idtoken",
  "bearertoken",
  "authtoken",
  "sessiontoken",
];

const DISPLAY_REDACTED_VALUE = "***";
const EDITING_REDACTED_PREFIX = "__AWAKEN_REDACTED_SECRET_";

type RedactedValueFactory = (path: readonly RedactionPathSegment[], original: unknown) => unknown;

function displayRedactedValue(): string {
  return DISPLAY_REDACTED_VALUE;
}

function editingRedactedValue(path: readonly RedactionPathSegment[]): string {
  return `${EDITING_REDACTED_PREFIX}${hashRedactionPath(path)}__`;
}

function hashRedactionPath(path: readonly RedactionPathSegment[]): string {
  const input = path
    .map((segment) => (typeof segment === "number" ? `[${segment}]` : segment))
    .join(".");
  let hash = 0x811c9dc5;
  for (let index = 0; index < input.length; index += 1) {
    hash ^= input.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}

const HEADER_CONTAINER_KEYS = new Set(["headers", "requestheaders", "responseheaders"]);

const SENSITIVE_HEADER_KEY_PATTERNS = [
  "authorization",
  "proxyauthorization",
  "cookie",
  "setcookie",
  "apikey",
  "xapikey",
  "xauthtoken",
  "token",
];

export type RedactionPathSegment = string | number;

export interface RedactionEntry {
  path: RedactionPathSegment[];
  original: unknown;
  redacted: unknown;
}

function normalizeSecretKey(key: string): string {
  return key.toLowerCase().replace(/[-_\s]/g, "");
}

function isSensitiveKey(key: string): boolean {
  const normalized = normalizeSecretKey(key);
  if (EXACT_SECRET_KEYS.has(normalized)) return true;
  return SENSITIVE_AUTH_KEY_PATTERNS.some((pattern) => normalized.includes(pattern));
}

function isSensitiveHeaderKey(parentKey: string, key: string): boolean {
  if (!HEADER_CONTAINER_KEYS.has(normalizeSecretKey(parentKey))) return false;
  const normalized = normalizeSecretKey(key);
  return SENSITIVE_HEADER_KEY_PATTERNS.some((pattern) => normalized.includes(pattern));
}

function cloneJsonValue<T>(value: T): T {
  if (value === undefined || value === null) return value;
  const encoded = JSON.stringify(value);
  if (encoded === undefined) return value;
  return JSON.parse(encoded) as T;
}

function recordRedaction(
  redactions: RedactionEntry[] | undefined,
  path: RedactionPathSegment[],
  original: unknown,
  redacted: unknown,
) {
  if (!redactions || deepEqualCanonical(original, redacted)) return;
  redactions.push({
    path: [...path],
    original: cloneJsonValue(original),
    redacted: cloneJsonValue(redacted),
  });
}

function redactRecord(
  value: unknown,
  parentKey: string = "",
  path: RedactionPathSegment[] = [],
  redactions?: RedactionEntry[],
  makeRedactedValue: RedactedValueFactory = displayRedactedValue,
): unknown {
  if (value === null || value === undefined) return value;
  // Primitive strings can still carry credentials even when their key name
  // is innocuous, such as tool output or upstream error messages.
  if (typeof value === "string") {
    const redacted = redactSecretString(value);
    if (redactions && !deepEqualCanonical(value, redacted)) {
      const redactedValue = makeRedactedValue(path, value);
      recordRedaction(redactions, path, value, redactedValue);
      return redactedValue;
    }
    recordRedaction(redactions, path, value, redacted);
    return redacted;
  }
  if (Array.isArray(value)) {
    return value.map((item, index) =>
      redactRecord(item, "", [...path, index], redactions, makeRedactedValue),
    );
  }
  if (typeof value === "object") {
    // Default-deny nested auth objects: RemoteAuth allows arbitrary keys, and
    // trace/audit payloads can contain auth-shaped data outside AgentSpec.
    if (normalizeSecretKey(parentKey) === "auth") {
      const safeAuth: Record<string, unknown> = {};
      for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
        if (key === "type" || inner === null || inner === undefined) {
          safeAuth[key] = inner;
        } else {
          const nextPath = [...path, key];
          const redacted = makeRedactedValue(nextPath, inner);
          safeAuth[key] = redacted;
          recordRedaction(redactions, nextPath, inner, redacted);
        }
      }
      return safeAuth;
    }
    const next: Record<string, unknown> = {};
    for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
      const nextPath = [...path, key];
      if (isSensitiveKey(key) || isSensitiveHeaderKey(parentKey, key)) {
        next[key] =
          inner === null || inner === undefined ? inner : makeRedactedValue(nextPath, inner);
        recordRedaction(redactions, nextPath, inner, next[key]);
      } else {
        next[key] = redactRecord(inner, key, nextPath, redactions, makeRedactedValue);
      }
    }
    return next;
  }
  return value;
}

/**
 * Return a copy of `endpoint` with secret-bearing fields masked to `"***"`.
 * The non-secret shape is preserved so read-only endpoint UI stays useful.
 */
export function redactEndpointForDisplay(endpoint: RemoteEndpoint): RemoteEndpoint {
  const generic = redactRecord(endpoint) as RemoteEndpoint;
  if (!generic.auth || typeof generic.auth !== "object") {
    return generic;
  }
  const auth = generic.auth as Record<string, unknown>;
  const safeAuth: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(auth)) {
    if (key === "type") {
      safeAuth[key] = value;
    } else if (value === null || value === undefined) {
      safeAuth[key] = value;
    } else {
      safeAuth[key] = DISPLAY_REDACTED_VALUE;
    }
  }
  return { ...generic, auth: safeAuth as RemoteEndpoint["auth"] };
}

/**
 * Recursively mask every secret-keyed field in an arbitrary value tree.
 * Used by display paths that serialize structures we don't statically know:
 * audit snapshots, persisted trace events, diffs, and restore previews.
 */
export function redactSecretsForDisplay<T>(value: T): T {
  return redactRecord(value) as T;
}

export interface RedactedAgentSpecForEditing {
  redacted: AgentSpec;
  redactions: RedactionEntry[];
}

export function redactAgentSpecForEditing(spec: AgentSpec): RedactedAgentSpecForEditing {
  const redactions: RedactionEntry[] = [];
  return {
    redacted: redactRecord(spec, "", [], redactions, editingRedactedValue) as AgentSpec,
    redactions,
  };
}

function readPath(root: unknown, path: readonly RedactionPathSegment[]) {
  let current = root;
  for (const segment of path) {
    if (current === null || typeof current !== "object") {
      return { found: false, value: undefined };
    }
    if (!Object.prototype.hasOwnProperty.call(current, segment)) {
      return { found: false, value: undefined };
    }
    current = Array.isArray(current)
      ? current[Number(segment)]
      : (current as Record<string, unknown>)[String(segment)];
  }
  return { found: true, value: current };
}

function writePath(root: unknown, path: readonly RedactionPathSegment[], value: unknown): boolean {
  if (path.length === 0) return false;
  let current = root;
  for (const segment of path.slice(0, -1)) {
    if (current === null || typeof current !== "object") return false;
    current = Array.isArray(current)
      ? current[Number(segment)]
      : (current as Record<string, unknown>)[String(segment)];
  }
  if (current === null || typeof current !== "object") return false;
  const leaf = path[path.length - 1];
  if (Array.isArray(current)) {
    current[Number(leaf)] = cloneJsonValue(value);
  } else {
    (current as Record<string, unknown>)[String(leaf)] = cloneJsonValue(value);
  }
  return true;
}

export function restoreUnchangedRedactions<T>(parsed: T, redactions: readonly RedactionEntry[]): T {
  if (redactions.length === 0) return parsed;
  const restored = cloneJsonValue(parsed);
  for (const redaction of redactions) {
    const current = readPath(restored, redaction.path);
    if (current.found && deepEqualCanonical(current.value, redaction.redacted)) {
      writePath(restored, redaction.path, redaction.original);
    }
  }
  return restored;
}

export interface RedactedFieldChange {
  path: string;
  before: unknown;
  after: unknown;
  redactedValueChanged: boolean;
}

/**
 * Compute semantic changes from raw values, but return only redacted
 * before/after payloads for rendering. This preserves secret-only changes
 * in DiffModal without handing the DOM the original credential values.
 */
export function computeRedactedDiff(
  prev: Record<string, unknown>,
  curr: Record<string, unknown>,
  base = "",
): RedactedFieldChange[] {
  return computeRedactedDiffFrom(
    prev,
    curr,
    redactSecretsForDisplay(prev),
    redactSecretsForDisplay(curr),
    base,
  );
}

function computeRedactedDiffFrom(
  prev: Record<string, unknown>,
  curr: Record<string, unknown>,
  redactedPrev: Record<string, unknown>,
  redactedCurr: Record<string, unknown>,
  base: string,
): RedactedFieldChange[] {
  const out: RedactedFieldChange[] = [];
  const keys = new Set([...Object.keys(prev ?? {}), ...Object.keys(curr ?? {})]);
  for (const key of keys) {
    const path = base ? `${base}.${key}` : key;
    const beforeRaw = prev?.[key];
    const afterRaw = curr?.[key];
    const beforeDisplay = redactedPrev?.[key];
    const afterDisplay = redactedCurr?.[key];
    if (deepEqualCanonical(beforeRaw, afterRaw)) continue;
    if (
      isDiffRecord(beforeRaw) &&
      isDiffRecord(afterRaw) &&
      isDiffRecord(beforeDisplay) &&
      isDiffRecord(afterDisplay)
    ) {
      out.push(...computeRedactedDiffFrom(beforeRaw, afterRaw, beforeDisplay, afterDisplay, path));
    } else {
      out.push({
        path,
        before: beforeDisplay,
        after: afterDisplay,
        redactedValueChanged: deepEqualCanonical(beforeDisplay, afterDisplay),
      });
    }
  }
  return out;
}

function isDiffRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Mask common credential patterns embedded in arbitrary text. Used by display
 * paths that render raw string payloads where key-based redaction cannot apply.
 */
export function redactSecretString(input: string): string {
  if (typeof input !== "string" || input.length === 0) return input;
  let result = input;
  result = result.replace(/Authorization\s*:\s*[^\r\n]+/gi, "Authorization: ***");
  result = result.replace(/Set-Cookie\s*:\s*[^\r\n]+/gi, "Set-Cookie: ***");
  result = result.replace(/(^|\s|;)Cookie\s*:\s*[^\r\n]+/gi, "$1Cookie: ***");
  result = result.replace(/Bearer\s+[A-Za-z0-9._\-+/=]{8,}/gi, "Bearer ***");
  result = result.replace(
    /\b(api[_-]?key|access[_-]?key|access[_-]?token|client[_-]?secret|refresh[_-]?token|id[_-]?token|bearer[_-]?token|password|secret|token|jwt)\s*[:=]\s*(?!\*\*\*)[^\s,;"'&}]+/gi,
    "$1=***",
  );
  result = result.replace(/\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g, "***");
  result = result.replace(/\bsk-[A-Za-z0-9_-]{16,}/g, "***");
  result = result.replace(/\bsk_(?:live|test)_[A-Za-z0-9]+/g, "***");
  result = result.replace(/\bgh[opsur]_[A-Za-z0-9_]{20,}\b/g, "***");
  result = result.replace(/\bgithub_pat_[A-Za-z0-9_]{20,}\b/g, "***");
  result = result.replace(/\bxox[aboprs]-[A-Za-z0-9-]{10,}\b/g, "***");
  result = result.replace(/\bxapp-[A-Za-z0-9-]{10,}\b/g, "***");
  result = result.replace(/\b(?:AKIA|ASIA)[A-Z0-9]{16}\b/g, "***");
  result = result.replace(
    /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g,
    "***",
  );
  return result;
}

/**
 * Return a copy of `spec` safe for display in the admin DOM: same shape,
 * but `endpoint` is run through `redactEndpointForDisplay`.
 */
export function redactAgentSpecForDisplay(spec: AgentSpec): AgentSpec {
  if (!spec.endpoint) return spec;
  return { ...spec, endpoint: redactEndpointForDisplay(spec.endpoint) };
}
