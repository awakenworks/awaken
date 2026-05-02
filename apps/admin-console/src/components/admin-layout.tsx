import { Outlet } from "react-router";
import { AdminSidebar } from "./admin-sidebar";
import { AdminTopbar } from "./admin-topbar";
import { CommandPaletteProvider } from "./command-palette";

export function AdminLayout() {
  return (
    <CommandPaletteProvider>
      <div className="min-h-screen bg-bg text-fg md:flex">
        <AdminSidebar />
        <div className="flex min-w-0 flex-1 flex-col">
          <AdminTopbar />
          <main className="flex-1">
            <Outlet />
          </main>
        </div>
      </div>
    </CommandPaletteProvider>
  );
}
