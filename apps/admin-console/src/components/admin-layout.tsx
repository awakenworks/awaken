import { useEffect, useState } from "react";
import { Outlet, useLocation } from "react-router";
import { AdminSidebar } from "./admin-sidebar";
import { AdminTopbar } from "./admin-topbar";
import { CommandPaletteProvider } from "./command-palette";

export function AdminLayout() {
  const [drawerOpen, setDrawerOpen] = useState(false);
  const { pathname } = useLocation();

  // Auto-close the mobile drawer on route change.
  useEffect(() => {
    setDrawerOpen(false);
  }, [pathname]);

  // Lock body scroll when drawer is open on mobile.
  useEffect(() => {
    if (drawerOpen) {
      document.body.style.overflow = "hidden";
      return () => {
        document.body.style.overflow = "";
      };
    }
  }, [drawerOpen]);

  return (
    <CommandPaletteProvider>
      <div className="min-h-screen bg-bg text-fg md:flex">
        <AdminSidebar drawerOpen={drawerOpen} onCloseDrawer={() => setDrawerOpen(false)} />
        {drawerOpen && (
          <button
            type="button"
            aria-label="Close menu"
            onClick={() => setDrawerOpen(false)}
            className="fixed inset-0 z-30 bg-fg-strong/40 backdrop-blur-sm md:hidden"
          />
        )}
        <div className="flex min-w-0 flex-1 flex-col">
          <AdminTopbar onOpenDrawer={() => setDrawerOpen(true)} />
          <main className="flex-1">
            <Outlet />
          </main>
        </div>
      </div>
    </CommandPaletteProvider>
  );
}
