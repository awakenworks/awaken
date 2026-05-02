import { useEffect, useState } from "react";
import { configApi, type McpServerRecord, type ProviderRecord } from "./config-api";
import type { NavHealthSource } from "./nav";

export type HealthTone = "ok" | "warn" | "error" | "neutral";

export interface NavHealth {
  count?: number;
  tone: HealthTone;
  hint?: string;
}

export interface NavHealthMap {
  mcp: NavHealth;
  providers: NavHealth;
  agents: NavHealth;
}

const NEUTRAL: NavHealth = { tone: "neutral" };

/** Lightweight health probe. Lists the relevant namespace once per mount and
 *  derives a count + tone. Failures fall back to neutral so the chrome never
 *  blocks on flaky data. */
export function useNavHealth(enabled: boolean): NavHealthMap {
  const [health, setHealth] = useState<NavHealthMap>({
    mcp: NEUTRAL,
    providers: NEUTRAL,
    agents: NEUTRAL,
  });

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    void Promise.all([
      configApi.list<McpServerRecord>("mcp-servers", 0, 100).catch(() => null),
      configApi.list<ProviderRecord>("providers", 0, 100).catch(() => null),
      configApi.list<unknown>("agents", 0, 100).catch(() => null),
    ]).then(([mcp, providers, agents]) => {
      if (cancelled) return;
      setHealth({
        mcp: deriveMcpHealth(mcp?.items),
        providers: deriveProviderHealth(providers?.items),
        agents: countOnly(agents?.items),
      });
    });
    return () => {
      cancelled = true;
    };
  }, [enabled]);

  return health;
}

function deriveMcpHealth(items: McpServerRecord[] | undefined): NavHealth {
  if (!items) return NEUTRAL;
  if (items.length === 0) return { count: 0, tone: "neutral" };
  const disabled = items.filter((s) => !s.enabled).length;
  if (disabled > 0) {
    return {
      count: items.length,
      tone: "warn",
      hint: `${disabled} server${disabled === 1 ? "" : "s"} disabled`,
    };
  }
  return { count: items.length, tone: "ok" };
}

function deriveProviderHealth(items: ProviderRecord[] | undefined): NavHealth {
  if (!items) return NEUTRAL;
  if (items.length === 0) return { count: 0, tone: "neutral" };
  return { count: items.length, tone: "ok" };
}

function countOnly(items: unknown[] | undefined): NavHealth {
  if (!items) return NEUTRAL;
  return { count: items.length, tone: "neutral" };
}

export const HEALTH_TONE_BG: Record<HealthTone, string> = {
  ok: "bg-state-done",
  warn: "bg-state-progress",
  error: "bg-state-blocked",
  neutral: "bg-fg-faint",
};

export function pickHealth(map: NavHealthMap, source: NavHealthSource | undefined): NavHealth {
  if (!source) return NEUTRAL;
  return map[source];
}
