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
            className="fixed inset-0 z-30 bg-overlay backdrop-blur-sm md:hidden"
          />
        )}
        <div className="flex min-w-0 flex-1 flex-col">
          <AdminTopbar onOpenDrawer={() => setDrawerOpen(true)} />
          {/* `min-w-0` lets `<main>` shrink below the natural width of
              its children so flex layout works. Don't clip overflow
              globally: long JSON / wide tables / trace payloads must
              be horizontally scrollable inside their own container,
              not hidden by an outer wrapper. Pages that need local
              clipping wrap the offending child in `overflow-x-auto`
              (see JsonInspector, RecentTracesDrawer, tables). */}
          <main className="min-w-0 flex-1">
            <Outlet />
          </main>
        </div>
      </div>
    </CommandPaletteProvider>
  );
}
