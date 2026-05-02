import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useNavigate } from "react-router";
import { configApi, type AgentSpec, type ToolInfo } from "@/lib/config-api";
import { navGroups } from "@/lib/nav";
import { adminRoutes } from "@/lib/routes";

export interface CommandItem {
  id: string;
  label: string;
  hint?: string;
  group: string;
  action: () => void;
  keywords?: string;
}

interface PaletteContextValue {
  open: () => void;
  close: () => void;
  isOpen: boolean;
}

const PaletteContext = createContext<PaletteContextValue | null>(null);

export function useCommandPalette(): PaletteContextValue {
  const ctx = useContext(PaletteContext);
  if (!ctx) {
    throw new Error("useCommandPalette must be used inside <CommandPaletteProvider>");
  }
  return ctx;
}

export function CommandPaletteProvider({ children }: { children: ReactNode }) {
  const [isOpen, setIsOpen] = useState(false);
  const open = useCallback(() => setIsOpen(true), []);
  const close = useCallback(() => setIsOpen(false), []);
  const value = useMemo(() => ({ open, close, isOpen }), [open, close, isOpen]);

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        setIsOpen((current) => !current);
      } else if (e.key === "Escape" && isOpen) {
        e.preventDefault();
        setIsOpen(false);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [isOpen]);

  return (
    <PaletteContext.Provider value={value}>
      {children}
      {isOpen && <PaletteOverlay onClose={close} />}
    </PaletteContext.Provider>
  );
}

function PaletteOverlay({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate();
  const inputRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState("");
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [highlight, setHighlight] = useState(0);

  useEffect(() => {
    inputRef.current?.focus();
    let cancelled = false;
    void Promise.all([
      configApi.list<AgentSpec>("agents", 0, 100).catch(() => null),
      configApi.capabilities().catch(() => null),
    ]).then(([agentsRes, caps]) => {
      if (cancelled) return;
      if (agentsRes) setAgents(agentsRes.items);
      if (caps) setTools(caps.tools);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const items: CommandItem[] = useMemo(() => {
    const out: CommandItem[] = [];
    // Navigation: every nav item.
    for (const group of navGroups) {
      for (const item of group.items) {
        out.push({
          id: `nav:${item.id}`,
          label: item.label,
          hint: `Go to ${item.label}`,
          group: `Go to · ${group.label}`,
          keywords: `${item.id} ${item.label.toLowerCase()}`,
          action: () => {
            navigate(item.path);
            onClose();
          },
        });
      }
    }
    // Quick action.
    out.push({
      id: "action:new-agent",
      label: "New agent",
      hint: "Open the editor for a new agent",
      group: "Actions",
      keywords: "create agent new",
      action: () => {
        navigate(adminRoutes.agentNew);
        onClose();
      },
    });
    // Agents (open editor).
    for (const agent of agents.slice(0, 50)) {
      out.push({
        id: `agent:${agent.id}`,
        label: agent.id,
        hint: `Edit ${agent.id} (model: ${agent.model_id})`,
        group: "Agents",
        keywords: `${agent.id} ${agent.model_id}`.toLowerCase(),
        action: () => {
          navigate(adminRoutes.agent(agent.id));
          onClose();
        },
      });
    }
    // Tools (no nav, displayed informationally — opens the assistant for now).
    for (const tool of tools.slice(0, 50)) {
      out.push({
        id: `tool:${tool.id}`,
        label: tool.id,
        hint: tool.description ?? tool.name,
        group: "Tools",
        keywords: `${tool.id} ${tool.name ?? ""} ${tool.description ?? ""}`.toLowerCase(),
        action: () => {
          navigate(adminRoutes.assistant);
          onClose();
        },
      });
    }
    return out;
  }, [agents, tools, navigate, onClose]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return items.slice(0, 24);
    return items
      .filter((item) => {
        const hay = `${item.label} ${item.keywords ?? ""} ${item.hint ?? ""}`.toLowerCase();
        return hay.includes(q);
      })
      .slice(0, 50);
  }, [items, query]);

  // Group adjacent results by `group`.
  const sections = useMemo(() => {
    const out: Array<{ label: string; items: CommandItem[] }> = [];
    for (const item of filtered) {
      const last = out[out.length - 1];
      if (last && last.label === item.group) {
        last.items.push(item);
      } else {
        out.push({ label: item.group, items: [item] });
      }
    }
    return out;
  }, [filtered]);

  // Reset highlight when results change.
  useEffect(() => {
    setHighlight(0);
  }, [query]);

  function handleKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlight((h) => Math.min(h + 1, filtered.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlight((h) => Math.max(h - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const target = filtered[highlight];
      if (target) target.action();
    }
  }

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      className="fixed inset-0 z-50 flex items-start justify-center bg-fg-strong/40 px-4 pt-[12vh] backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-full max-w-2xl overflow-hidden rounded-lg bg-surface shadow-overlay"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-3 border-b border-line px-4 py-3">
          <span aria-hidden className="text-fg-faint">⌘K</span>
          <input
            ref={inputRef}
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search agents, tools, navigation, actions…"
            className="flex-1 bg-transparent text-sm text-fg-strong outline-none placeholder:text-fg-faint"
          />
          <kbd className="rounded border border-line bg-soft px-1.5 py-0.5 font-mono text-[10px] text-fg-faint">
            esc
          </kbd>
        </div>

        <div className="max-h-[60vh] overflow-y-auto">
          {filtered.length === 0 ? (
            <div className="px-6 py-10 text-center text-sm text-fg-soft">
              No matches.
            </div>
          ) : (
            sections.map((section, sIdx) => {
              return (
                <div key={`${section.label}-${sIdx}`} className="py-2">
                  <div className="px-4 pb-1 text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
                    {section.label}
                  </div>
                  {section.items.map((item) => {
                    const flatIndex = filtered.indexOf(item);
                    const active = flatIndex === highlight;
                    return (
                      <button
                        key={item.id}
                        type="button"
                        onMouseEnter={() => setHighlight(flatIndex)}
                        onClick={item.action}
                        className={[
                          "flex w-full items-center justify-between gap-3 px-4 py-2 text-left text-sm",
                          active ? "bg-soft text-fg-strong" : "text-fg hover:bg-soft",
                        ].join(" ")}
                      >
                        <span className="truncate font-medium">{item.label}</span>
                        {item.hint && (
                          <span className="ml-2 truncate text-xs text-fg-soft">
                            {item.hint}
                          </span>
                        )}
                      </button>
                    );
                  })}
                </div>
              );
            })
          )}
        </div>

        <div className="flex items-center gap-3 border-t border-line bg-soft px-4 py-2 text-[11px] text-fg-faint">
          <span>
            <kbd className="rounded border border-line bg-bg px-1 font-mono">↑</kbd>
            <kbd className="ml-0.5 rounded border border-line bg-bg px-1 font-mono">↓</kbd>{" "}
            navigate
          </span>
          <span>
            <kbd className="rounded border border-line bg-bg px-1 font-mono">↵</kbd>{" "}
            open
          </span>
          <span className="ml-auto">
            {filtered.length} result{filtered.length === 1 ? "" : "s"}
          </span>
        </div>
      </div>
    </div>
  );
}
