import { Fragment } from "react";
import { useTranslation } from "react-i18next";
import { Link, useLocation } from "react-router";
import { resolveBreadcrumbs } from "@/lib/nav";
import { maskAdminToken } from "@/lib/admin-token";
import { useTheme, type ThemeChoice } from "@/lib/use-theme";
import { setLocale, currentLocale, type Locale } from "@/lib/i18n";
import { describeAuthStatus, useAuth } from "./auth-provider";
import { useCommandPalette } from "./command-palette";

const STATUS_DOT: Record<"ok" | "warn" | "error" | "neutral", string> = {
  ok: "bg-state-done",
  warn: "bg-state-progress",
  error: "bg-state-blocked",
  neutral: "bg-fg-faint",
};

export function AdminTopbar({
  onOpenDrawer,
}: {
  onOpenDrawer?: () => void;
} = {}) {
  const { t } = useTranslation();
  const { pathname } = useLocation();
  const crumbs = resolveBreadcrumbs(pathname);
  const { token, status, openTokenModal } = useAuth();
  const description = describeAuthStatus(status);
  const dotClass = STATUS_DOT[description.tone];
  const palette = useCommandPalette();

  return (
    <header className="sticky top-0 z-20 flex h-14 items-center gap-4 border-b border-line bg-canvas/80 px-4 backdrop-blur md:px-6">
      {onOpenDrawer && (
        <button
          type="button"
          aria-label="Open menu"
          onClick={onOpenDrawer}
          className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-md text-fg-soft transition-colors hover:bg-soft md:hidden"
        >
          <svg
            aria-hidden
            viewBox="0 0 24 24"
            className="h-5 w-5"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
          >
            <line x1="4" y1="6" x2="20" y2="6" />
            <line x1="4" y1="12" x2="20" y2="12" />
            <line x1="4" y1="18" x2="20" y2="18" />
          </svg>
        </button>
      )}
      <nav aria-label="Breadcrumb" className="flex min-w-0 flex-1 items-center gap-2 text-sm text-fg-soft">
        {crumbs.map((crumb, idx) => {
          const isLast = idx === crumbs.length - 1;
          const text = crumb.labelKey ? t(crumb.labelKey) : crumb.label;
          return (
            <Fragment key={`${crumb.label}-${idx}`}>
              {idx > 0 && (
                <span aria-hidden className="text-fg-faint">
                  /
                </span>
              )}
              {crumb.path && !isLast ? (
                <Link
                  to={crumb.path}
                  className="rounded px-1 transition-colors hover:text-fg"
                >
                  {text}
                </Link>
              ) : (
                <span
                  className={
                    isLast
                      ? "truncate text-fg-strong font-medium"
                      : "truncate text-fg-soft"
                  }
                >
                  {text}
                </span>
              )}
            </Fragment>
          );
        })}
      </nav>

      <div className="flex items-center gap-2">
        <button
          type="button"
          aria-label="Open command palette"
          onClick={palette.open}
          className="hidden h-9 items-center gap-2 rounded-md border border-line bg-soft px-3 text-xs text-fg-soft transition-colors hover:border-line-strong md:inline-flex"
        >
          <span>{t("topbar.palettePlaceholder")}</span>
          <span className="flex items-center gap-1 text-fg-faint">
            <kbd className="rounded border border-line bg-bg px-1.5 py-0.5 font-mono text-[10px]">
              ⌘
            </kbd>
            <kbd className="rounded border border-line bg-bg px-1.5 py-0.5 font-mono text-[10px]">
              K
            </kbd>
          </span>
        </button>

        <LocaleToggle />

        <ThemeToggle />

        <button
          type="button"
          aria-label="Notifications (coming soon)"
          disabled
          className="hidden h-9 w-9 items-center justify-center rounded-md border border-line bg-soft text-fg-soft transition-colors hover:border-line-strong disabled:cursor-not-allowed disabled:opacity-60 md:inline-flex"
        >
          <svg
            aria-hidden
            viewBox="0 0 24 24"
            className="h-4 w-4"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
          >
            <path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" strokeLinecap="round" strokeLinejoin="round" />
            <path d="M10 21a2 2 0 0 0 4 0" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>

        <button
          type="button"
          onClick={openTokenModal}
          className="flex items-center gap-3 rounded-md border border-line-strong bg-surface px-3 py-1.5 text-left text-xs text-fg-soft transition-colors hover:bg-soft hover:text-fg"
        >
          <span className="flex items-center gap-1.5">
            <span aria-hidden className={`inline-block h-2 w-2 rounded-pill ${dotClass}`} />
            <span className="hidden sm:inline">{description.label}</span>
          </span>
          <span aria-hidden className="hidden h-3 w-px bg-line sm:inline-block" />
          <span className="hidden font-mono text-fg sm:inline">{maskAdminToken(token)}</span>
        </button>
      </div>
    </header>
  );
}

function LocaleToggle() {
  const { i18n } = useTranslation();
  const locale = (i18n.language as Locale) ?? currentLocale();
  const next: Locale = locale === "zh-CN" ? "en" : "zh-CN";
  return (
    <button
      type="button"
      aria-label={`Switch language to ${next}`}
      title={`Switch language to ${next}`}
      onClick={() => setLocale(next)}
      className="hidden h-9 items-center justify-center rounded-md border border-line-strong bg-surface px-2.5 text-xs font-medium text-fg-soft transition-colors hover:bg-soft hover:text-fg md:inline-flex"
    >
      {locale === "zh-CN" ? "中" : "EN"}
    </button>
  );
}

function ThemeToggle() {
  const { choice, cycle } = useTheme();
  const label = `Theme: ${describeChoice(choice)} (click to change)`;
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={cycle}
      className="hidden h-9 w-9 items-center justify-center rounded-md border border-line bg-soft text-fg-soft transition-colors hover:border-line-strong hover:text-fg md:inline-flex"
    >
      {choice === "light" && <SunIcon />}
      {choice === "dark" && <MoonIcon />}
      {choice === "system" && <SystemIcon />}
    </button>
  );
}

function describeChoice(choice: ThemeChoice): string {
  if (choice === "light") return "light";
  if (choice === "dark") return "dark";
  return "system";
}

function SunIcon() {
  return (
    <svg aria-hidden viewBox="0 0 24 24" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
    </svg>
  );
}

function MoonIcon() {
  return (
    <svg aria-hidden viewBox="0 0 24 24" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M21 12.79A9 9 0 1 1 11.21 3a7 7 0 0 0 9.79 9.79z" />
    </svg>
  );
}

function SystemIcon() {
  return (
    <svg aria-hidden viewBox="0 0 24 24" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="3" y="4" width="18" height="13" rx="2" />
      <path d="M8 21h8M12 17v4" />
    </svg>
  );
}
