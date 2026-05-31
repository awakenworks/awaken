// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Link, MemoryRouter, Route, Routes } from "react-router";
import { AdminLayout } from "./admin-layout";

vi.mock("./admin-sidebar", () => ({
  AdminSidebar: ({ drawerOpen, onCloseDrawer }: { drawerOpen: boolean; onCloseDrawer: () => void }) => (
    <aside data-open={drawerOpen ? "true" : "false"}>
      <button type="button" onClick={onCloseDrawer}>sidebar close</button>
    </aside>
  ),
}));

vi.mock("./admin-topbar", () => ({
  AdminTopbar: ({ onOpenDrawer }: { onOpenDrawer: () => void }) => (
    <button type="button" onClick={onOpenDrawer}>open drawer</button>
  ),
}));

vi.mock("./command-palette", () => ({
  CommandPaletteProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

vi.mock("./admin-assistant-drawer", () => ({
  AdminAssistantDrawer: () => <div data-testid="admin-assistant-drawer" />,
}));

function renderLayout(initialEntry = "/") {
  return render(
    <MemoryRouter initialEntries={[initialEntry]}>
      <Routes>
        <Route element={<AdminLayout />}>
          <Route index element={<Link to="/next">go next</Link>} />
          <Route path="next" element={<div>next page</div>} />
        </Route>
      </Routes>
    </MemoryRouter>,
  );
}

afterEach(() => {
  cleanup();
  document.body.style.overflow = "";
});

describe("AdminLayout", () => {
  it("opens and closes the mobile drawer while locking body scroll", async () => {
    const { unmount } = renderLayout();

    expect(screen.getByRole("complementary").getAttribute("data-open")).toBe("false");

    fireEvent.click(screen.getByRole("button", { name: "open drawer" }));
    expect(screen.getByRole("complementary").getAttribute("data-open")).toBe("true");
    expect(screen.getByRole("button", { name: "Close menu" })).toBeTruthy();
    expect(document.body.style.overflow).toBe("hidden");

    fireEvent.click(screen.getByRole("button", { name: "Close menu" }));
    await waitFor(() => expect(screen.getByRole("complementary").getAttribute("data-open")).toBe("false"));
    expect(document.body.style.overflow).toBe("");

    fireEvent.click(screen.getByRole("button", { name: "open drawer" }));
    expect(document.body.style.overflow).toBe("hidden");
    unmount();
    expect(document.body.style.overflow).toBe("");
  });

  it("auto-closes the drawer after route navigation", async () => {
    renderLayout();

    fireEvent.click(screen.getByRole("button", { name: "open drawer" }));
    expect(screen.getByRole("complementary").getAttribute("data-open")).toBe("true");

    fireEvent.click(screen.getByRole("link", { name: "go next" }));
    expect(await screen.findByText("next page")).toBeTruthy();
    await waitFor(() => expect(screen.getByRole("complementary").getAttribute("data-open")).toBe("false"));
  });
});
