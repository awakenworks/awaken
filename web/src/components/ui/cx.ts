// Tiny class-merge util (the oversight-next pattern): no clsx/cva dependency —
// components render a semantic `ui-*`/legacy class plus a typed variant/tone
// modifier, and all visual values live in CSS custom properties (tokens.css).

export type ClassValue = string | false | null | undefined;

export function cx(...values: ClassValue[]): string {
  return values.filter(Boolean).join(" ");
}

/** The shared tone union primitives key their color modifier on. */
export type Tone = "ok" | "warn" | "danger" | "agent" | "neutral" | "info";
