import { useTranslation } from "react-i18next";
import { NavLink } from "react-router";
import { BACKEND_URL } from "@/lib/config-api";
import { navGroups, type NavBadge, type NavItem } from "@/lib/nav";
import { HEALTH_TONE_BG, pickHealth, useNavHealth } from "@/lib/use-nav-health";
import { useSystemInfo } from "@/lib/use-system-info";
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
  const { t } = useTranslation();
  const { status, openTokenModal } = useAuth();
  const description = describeAuthStatus(status);
  const dotClass = STATUS_DOT[description.tone];
  const health = useNavHealth(status === "ok");
  const sysInfo = useSystemInfo();

  return (
    <aside
      data-open={drawerOpen ? "true" : "false"}
      className={[
        // Mobile: fixed drawer that slides in from left when [data-open=true]
        "fixed inset-y-0 left-0 z-40 flex w-[264px] flex-col bg-canvas text-fg shadow-overlay transition-transform duration-fast",
        "data-[open=false]:-translate-x-full data-[open=true]:translate-x-0",
        // Desktop: static sidebar, no drawer behavior
        "md:static md:translate-x-0 md:min-h-screen md:border-r md:border-line md:shadow-none md:data-[open=false]:translate-x-0",
      ].join(" ")}
    >
      <div className="border-b border-line px-6 py-6">
        <p className="text-[11px] font-medium uppercase tracking-[0.18em] text-fg-soft">
          {t("app.eyebrow")}
        </p>
        <h1 className="mt-2 text-2xl font-semibold tracking-tight text-fg-strong">
          {t("app.title")}
        </h1>

        <button
          type="button"
          onClick={openTokenModal}
          className="mt-5 block w-full rounded-md border border-line bg-soft p-3 text-left transition-colors hover:border-line-strong"
        >
          <div className="flex items-center justify-between text-[10px] font-medium uppercase tracking-[0.18em]">
            <span className="text-fg-soft">{t("app.connectedBackend")}</span>
            <span className="flex items-center gap-1.5 text-fg-soft">
              <span aria-hidden className={`inline-block h-1.5 w-1.5 rounded-pill ${dotClass}`} />
              <span className="text-[10px] tracking-normal normal-case">
                {description.label}
              </span>
            </span>
          </div>
          <div className="mt-1.5 break-all font-mono text-[11px] text-fg">
            {BACKEND_URL}
          </div>
        </button>
      </div>

      <nav className="flex flex-1 flex-col space-y-4 px-3 py-5">
        {navGroups.map((group) => (
          <div key={group.label} className="contents">
            <div
              aria-hidden
              className="px-3 pb-1 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft"
            >
              {t(group.groupKey)}
            </div>
            <div className="flex flex-col gap-0.5">
              {group.items.map((item) => (
                <SidebarLink
                  key={item.id}
                  item={item}
                  health={pickHealth(health, item.healthSource)}
                  onNavigate={onCloseDrawer}
                />
              ))}
            </div>
          </div>
        ))}
      </nav>

      <div className="border-t border-line px-5 py-3 space-y-1.5 text-[10px] text-fg-soft">
        <div className="flex items-center gap-2 font-mono">
          {sysInfo && (
            <span
              aria-hidden
              className="inline-block h-1.5 w-1.5 animate-pulse rounded-pill bg-state-done"
              title={`uptime ${Math.floor(sysInfo.uptime_seconds / 60)}m`}
            />
          )}
          <span>v{sysInfo?.version ?? "—"} · 9-phase loop</span>
        </div>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
          <span className="inline-flex items-center gap-1">
            <kbd className="rounded border border-line bg-bg px-1 font-mono text-[9px]">⌘K</kbd>
            search
          </span>
          <span className="text-fg-faint">·</span>
          <span className="inline-flex items-center gap-1">
            <kbd className="rounded border border-line bg-bg px-1 font-mono text-[9px]">G</kbd>
            then
            <kbd className="rounded border border-line bg-bg px-1 font-mono text-[9px]">A</kbd>
            agents
          </span>
        </div>
      </div>
    </aside>
  );
}

function SidebarLink({
  item,
  health,
  onNavigate,
}: {
  item: NavItem;
  health: { count?: number; tone: "ok" | "warn" | "error" | "neutral"; hint?: string };
  onNavigate?: () => void;
}) {
  const { t } = useTranslation();
  const showHealth = item.healthSource !== undefined && health.tone !== "neutral";
  const dotBg = HEALTH_TONE_BG[health.tone];
  const label = item.labelKey ? t(item.labelKey) : item.label;
  return (
    <NavLink
      to={item.path}
      end={item.end}
      onClick={onNavigate}
      className={({ isActive }) =>
        [
          "group relative flex min-w-0 items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors",
          isActive
            ? "bg-soft text-fg-strong before:absolute before:left-0 before:top-1.5 before:bottom-1.5 before:w-[2px] before:rounded-pill before:bg-accent"
            : "text-fg-soft hover:bg-soft/60 hover:text-fg",
        ].join(" ")
      }
    >
      <span className="flex-1 truncate">{label}</span>
      {showHealth && (
        <span
          aria-label={health.hint ?? `${label} health: ${health.tone}`}
          title={health.hint}
          className={`inline-block h-1.5 w-1.5 rounded-pill ${dotBg}`}
        />
      )}
      {typeof health.count === "number" && health.count > 0 && (
        <span className="rounded bg-soft px-1.5 font-mono text-[10px] text-fg-soft group-[.active]:text-fg">
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
    <span className="rounded-pill bg-soft px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-fg-soft">
      ro
    </span>
  );
}
