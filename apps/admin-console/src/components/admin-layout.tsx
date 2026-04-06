import { NavLink, Outlet } from "react-router";
import { BACKEND_URL } from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";

const navItems = [
  { path: adminRoutes.dashboard, label: "Dashboard", end: true },
  { path: adminRoutes.agents, label: "Agents" },
  { path: adminRoutes.skills, label: "Skill Registry" },
  { path: adminRoutes.models, label: "Models" },
  { path: adminRoutes.providers, label: "Providers" },
  { path: adminRoutes.mcpServers, label: "MCP Servers" },
  { path: adminRoutes.assistant, label: "AI Assistant" },
];

export function AdminLayout() {
  return (
    <div className="min-h-screen text-slate-900 md:flex">
      <aside className="border-b border-slate-200 bg-[#102236] text-slate-100 md:min-h-screen md:w-80 md:border-b-0 md:border-r md:border-slate-800">
        <div className="border-b border-slate-800/80 px-6 py-6">
          <div>
            <p className="text-xs uppercase tracking-[0.26em] text-cyan-200/70">
              Awaken Control Plane
            </p>
            <h1 className="mt-2 text-3xl font-semibold text-white">
              Admin Console
            </h1>
            <p className="mt-3 text-sm leading-6 text-slate-300">
              Publish runtime-safe changes for agents, providers, models, and
              MCP servers against the live backend.
            </p>
          </div>

          <div className="mt-5 rounded-2xl border border-white/10 bg-white/5 p-4">
            <div className="text-[11px] uppercase tracking-[0.2em] text-slate-400">
              Connected Backend
            </div>
            <div className="mt-2 break-all font-mono text-xs text-slate-200">
              {BACKEND_URL}
            </div>
          </div>
        </div>

        <nav className="grid grid-cols-2 gap-2 px-4 py-4 sm:grid-cols-3 md:flex md:flex-col md:space-y-1 md:px-4 md:py-5">
          {navItems.map((item) => (
            <NavLink
              key={item.path}
              to={item.path}
              end={item.end}
              className={({ isActive }) =>
                [
                  "min-w-0 rounded-2xl px-4 py-3 text-left text-sm font-medium leading-5 whitespace-normal break-words transition",
                  isActive
                    ? "bg-[#f4efe6] text-slate-950 shadow-[0_16px_36px_rgba(6,17,29,0.28)]"
                    : "text-slate-300 hover:bg-white/10 hover:text-white",
                ].join(" ")
              }
            >
              {item.label}
            </NavLink>
          ))}
        </nav>
      </aside>

      <main className="min-h-screen flex-1">
        <Outlet />
      </main>
    </div>
  );
}
