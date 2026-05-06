import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { configResourceApi, type McpServerRecord, type ProviderRecord } from "../../api";
import type { NavHealthSource } from "../../nav";
import { qk } from "../keys";

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
const EMPTY_HEALTH: NavHealthMap = {
  mcp: NEUTRAL,
  providers: NEUTRAL,
  agents: NEUTRAL,
};

interface NavHealthPayload {
  mcp: McpServerRecord[] | undefined;
  providers: ProviderRecord[] | undefined;
  agents: unknown[] | undefined;
}

/** Lightweight health probe. Lists the relevant namespaces and derives count + tone.
 *  Failures fall back to neutral so the chrome never blocks on flaky data. */
export function useNavHealth(enabled: boolean): NavHealthMap {
  const query = useQuery<NavHealthPayload>({
    queryKey: qk.navHealth(),
    queryFn: async () => {
      const [mcp, providers, agents] = await Promise.all([
        configResourceApi.list<McpServerRecord>("mcp-servers", 0, 100).catch(() => null),
        configResourceApi.list<ProviderRecord>("providers", 0, 100).catch(() => null),
        configResourceApi.list<unknown>("agents", 0, 100).catch(() => null),
      ]);
      return {
        mcp: mcp?.items,
        providers: providers?.items,
        agents: agents?.items,
      };
    },
    enabled,
    staleTime: 30_000,
  });

  return useMemo(() => {
    if (!enabled || !query.data) return EMPTY_HEALTH;
    return {
      mcp: deriveMcpHealth(query.data.mcp),
      providers: deriveProviderHealth(query.data.providers),
      agents: countOnly(query.data.agents),
    };
  }, [enabled, query.data]);
}

function deriveMcpHealth(items: McpServerRecord[] | undefined): NavHealth {
  if (!items) return NEUTRAL;
  // The list payload doesn't carry a real "connected?" signal — that requires
  // a per-server /v1/mcp-servers/:id/status probe. Show count only here, no
  // synthetic tone.
  return { count: items.length, tone: "neutral" };
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
