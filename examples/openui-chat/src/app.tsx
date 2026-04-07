import { Navigate, NavLink, Route, Routes } from "react-router";
import { FullScreenPage } from "./pages/fullscreen";
import { CopilotPage } from "./pages/copilot";
import { BottomTrayPage } from "./pages/bottom-tray";

const navItems = [
  { to: "/fullscreen", label: "FullScreen" },
  { to: "/copilot", label: "Copilot" },
  { to: "/bottom-tray", label: "BottomTray" },
] as const;

function Nav() {
  return (
    <nav className="fixed top-0 left-0 right-0 z-50 flex items-center gap-2 bg-slate-900 px-4 py-2">
      <span className="mr-4 text-sm font-semibold text-white">
        OpenUI Chat
      </span>
      {navItems.map(({ to, label }) => (
        <NavLink
          key={to}
          to={to}
          className={({ isActive }) =>
            `rounded px-3 py-1 text-sm transition-colors ${
              isActive
                ? "bg-white text-slate-900"
                : "text-slate-300 hover:bg-slate-700 hover:text-white"
            }`
          }
        >
          {label}
        </NavLink>
      ))}
    </nav>
  );
}

export function App() {
  return (
    <>
      <Nav />
      <div className="pt-10">
        <Routes>
          <Route path="/" element={<Navigate to="/fullscreen" replace />} />
          <Route path="/fullscreen" element={<FullScreenPage />} />
          <Route path="/copilot" element={<CopilotPage />} />
          <Route path="/bottom-tray" element={<BottomTrayPage />} />
        </Routes>
      </div>
    </>
  );
}
