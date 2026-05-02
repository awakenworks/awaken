import { NavLink } from "react-router";
import { BACKEND_URL } from "@/lib/config-api";
import { navGroups, type NavBadge, type NavItem } from "@/lib/nav";
import { HEALTH_TONE_BG, pickHealth, useNavHealth } from "@/lib/use-nav-health";
import { describeAuthStatus, useAuth } from "./auth-provider";

const STATUS_DOT: Record<"ok" | "warn" | "error" | "neutral", string> = {
  ok: "bg-state-done",
  warn: "bg-state-progress",
  error: "bg-state-blocked",
  neutral: "bg-fg-faint",
};

export function AdminSidebar({
  drawerOpen = false,
  onCloseDrawer,
}: {
  drawerOpen?: boolean;
  onCloseDrawer?: () => void;
} = {}) {
  const { status, openTokenModal } = useAuth();
  const description = describeAuthStatus(status);
  const dotClass = STATUS_DOT[description.tone];
  const health = useNavHealth(status === "ok");

  return (
    <aside
      data-open={drawerOpen ? "true" : "false"}
      className={[
        // Mobile: fixed drawer that slides in from left when [data-open=true]
        "fixed inset-y-0 left-0 z-40 flex w-72 flex-col bg-chrome-bg text-chrome-fg shadow-overlay transition-transform duration-fast",
        "data-[open=false]:-translate-x-full data-[open=true]:translate-x-0",
        // Desktop: static sidebar, no drawer behavior
        "md:static md:translate-x-0 md:min-h-screen md:border-r md:border-chrome-line md:shadow-none md:data-[open=false]:translate-x-0",
      ].join(" ")}
    >
      <div className="border-b border-chrome-line px-6 py-6">
        <p className="text-[11px] font-medium uppercase tracking-[0.18em] text-chrome-eyebrow">
          Awaken Control Plane
        </p>
        <h1 className="mt-2 text-2xl font-semibold tracking-tight text-white">
          Admin Console
        </h1>

        <button
          type="button"
          onClick={openTokenModal}
          className="mt-5 block w-full rounded-md border border-chrome-line/80 bg-chrome-bg-2 p-3 text-left transition-colors hover:border-chrome-line"
        >
          <div className="flex items-center justify-between text-[10px] font-medium uppercase tracking-[0.18em]">
            <span className="text-chrome-fg-muted">Connected Backend</span>
            <span className="flex items-center gap-1.5 text-chrome-fg-muted">
              <span aria-hidden className={`inline-block h-1.5 w-1.5 rounded-pill ${dotClass}`} />
              <span className="text-[10px] tracking-normal normal-case">
                {description.label}
              </span>
            </span>
          </div>
          <div className="mt-1.5 break-all font-mono text-[11px] text-chrome-fg">
            {BACKEND_URL}
          </div>
        </button>
      </div>

      <nav className="flex flex-1 flex-col space-y-4 px-3 py-5">
        {navGroups.map((group) => (
          <div key={group.label} className="contents">
            <div
              aria-hidden
              className="px-3 pb-1 text-[10px] font-medium uppercase tracking-eyebrow text-chrome-fg-muted"
            >
              {group.label}
            </div>
            <div className="flex flex-col gap-0.5">
              {group.items.map((item) => (
                <SidebarLink
                  key={item.id}
                  item={item}
                  health={pickHealth(health, item.healthSource)}
                />
              ))}
            </div>
          </div>
        ))}
      </nav>

      <div className="border-t border-chrome-line px-5 py-3 text-[10px] font-mono text-chrome-fg-muted">
        v0.4.0 · 9-phase loop
      </div>
    </aside>
  );
}

function SidebarLink({
  item,
  health,
}: {
  item: NavItem;
  health: { count?: number; tone: "ok" | "warn" | "error" | "neutral"; hint?: string };
}) {
  const showHealth = item.healthSource !== undefined && health.tone !== "neutral";
  const dotBg = HEALTH_TONE_BG[health.tone];
  return (
    <NavLink
      to={item.path}
      end={item.end}
      className={({ isActive }) =>
        [
          "group flex min-w-0 items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors",
          isActive
            ? "bg-chrome-bg-2 text-white"
            : "text-chrome-fg-muted hover:bg-chrome-bg-2/60 hover:text-chrome-fg",
        ].join(" ")
      }
    >
      <span className="flex-1 truncate">{item.label}</span>
      {showHealth && (
        <span
          aria-label={health.hint ?? `${item.label} health: ${health.tone}`}
          title={health.hint}
          className={`inline-block h-1.5 w-1.5 rounded-pill ${dotBg}`}
        />
      )}
      {typeof health.count === "number" && health.count > 0 && (
        <span className="rounded bg-chrome-bg-2 px-1.5 font-mono text-[10px] text-chrome-fg-muted group-[.active]:text-chrome-fg">
          {health.count}
        </span>
      )}
      {item.badge && <NavBadgePill badge={item.badge} />}
    </NavLink>
  );
}

function NavBadgePill({ badge }: { badge: NavBadge }) {
  if (badge === "live") {
    return (
      <span className="flex items-center gap-1 rounded-pill bg-state-progress/20 px-2 py-0.5 text-[10px] font-medium text-state-progress">
        <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-pill bg-state-progress" />
        live
      </span>
    );
  }
  return (
    <span className="rounded-pill bg-chrome-bg-2 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-chrome-fg-muted">
      ro
    </span>
  );
}
