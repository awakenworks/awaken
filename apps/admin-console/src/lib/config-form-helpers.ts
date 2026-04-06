export function parseLineList(input: string): string[] {
  return input
    .split(/\r?\n/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

export function stringifyLineList(values: string[] | undefined): string {
  return (values ?? []).join("\n");
}

export function parseJsonObject<T extends Record<string, unknown>>(
  input: string,
  label: string,
): T {
  const trimmed = input.trim();
  const value = JSON.parse(trimmed.length === 0 ? "{}" : trimmed) as unknown;

  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be a JSON object`);
  }

  return value as T;
}

export function parseStringRecord(
  input: string,
  label: string,
): Record<string, string> {
  const value = parseJsonObject<Record<string, unknown>>(input, label);
  const entries = Object.entries(value);

  for (const [key, entryValue] of entries) {
    if (typeof entryValue !== "string") {
      throw new Error(`${label} value for "${key}" must be a string`);
    }
  }

  return Object.fromEntries(entries) as Record<string, string>;
}

export function stringifyJsonObject(
  value: Record<string, unknown> | undefined,
): string {
  if (!value || Object.keys(value).length === 0) {
    return "{}";
  }

  return JSON.stringify(value, null, 2);
}
