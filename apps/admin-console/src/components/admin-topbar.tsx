import { Fragment } from "react";
import { Link, useLocation } from "react-router";
import { resolveBreadcrumbs } from "@/lib/nav";
import { maskAdminToken } from "@/lib/admin-token";
import { describeAuthStatus, useAuth } from "./auth-provider";

const STATUS_DOT: Record<"ok" | "warn" | "error" | "neutral", string> = {
  ok: "bg-state-done",
  warn: "bg-state-progress",
  error: "bg-state-blocked",
  neutral: "bg-fg-faint",
};

export function AdminTopbar() {
  const { pathname } = useLocation();
  const crumbs = resolveBreadcrumbs(pathname);
  const { token, status, openTokenModal } = useAuth();
  const description = describeAuthStatus(status);
  const dotClass = STATUS_DOT[description.tone];

  return (
    <header className="sticky top-0 z-20 flex h-14 items-center gap-4 border-b border-line bg-canvas/80 px-6 backdrop-blur">
      <nav aria-label="Breadcrumb" className="flex min-w-0 flex-1 items-center gap-2 text-sm text-fg-soft">
        {crumbs.map((crumb, idx) => {
          const isLast = idx === crumbs.length - 1;
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
                  {crumb.label}
                </Link>
              ) : (
                <span
                  className={
                    isLast
                      ? "truncate text-fg-strong font-medium"
                      : "truncate text-fg-soft"
                  }
                >
                  {crumb.label}
                </span>
              )}
            </Fragment>
          );
        })}
      </nav>

      <div className="flex items-center gap-2">
        <button
          type="button"
          aria-label="Open command palette (coming soon)"
          disabled
          className="hidden h-9 items-center gap-2 rounded-md border border-line bg-soft px-3 text-xs text-fg-soft transition-colors hover:border-line-strong disabled:cursor-not-allowed disabled:opacity-60 md:inline-flex"
        >
          <span>Search agents, tools, runs…</span>
          <span className="flex items-center gap-1 text-fg-faint">
            <kbd className="rounded border border-line bg-bg px-1.5 py-0.5 font-mono text-[10px]">
              ⌘
            </kbd>
            <kbd className="rounded border border-line bg-bg px-1.5 py-0.5 font-mono text-[10px]">
              K
            </kbd>
          </span>
        </button>

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
          className="flex items-center gap-3 rounded-md border border-line bg-soft px-3 py-1.5 text-left text-xs text-fg-soft transition-colors hover:border-line-strong"
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
