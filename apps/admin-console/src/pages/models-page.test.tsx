// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { ModelsPage } from "./models-page";
import type { ModelBindingSpec } from "@/lib/config-api";
import type { SortState } from "@/lib/list-view";

const pageState = vi.hoisted(() => ({
  crud: {} as {
    items: ModelBindingSpec[];
    draft: ModelBindingSpec | null;
    loading: boolean;
    saving: boolean;
    error: string | null;
    isEditingExisting: boolean;
    auxiliaryData: string[];
    setDraft: ReturnType<typeof vi.fn>;
    setError: ReturnType<typeof vi.fn>;
    startEdit: ReturnType<typeof vi.fn>;
    startNew: ReturnType<typeof vi.fn>;
    cancelEdit: ReturnType<typeof vi.fn>;
    handleSave: ReturnType<typeof vi.fn>;
    handleDelete: ReturnType<typeof vi.fn>;
  },
  list: {
    search: "",
    sort: { key: "id", direction: "asc" } as SortState<"id" | "provider_id" | "upstream_model" | "updated_at">,
    pageSize: 10,
    page: 1,
    apply: vi.fn(),
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("@/lib/use-crud-page", () => ({
  useCrudPage: () => pageState.crud,
}));

vi.mock("@/lib/list-url-state", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/list-url-state")>();
  return {
    ...actual,
    useListUrlState: () => pageState.list,
  };
});

function model(overrides: Partial<ModelBindingSpec>): ModelBindingSpec {
  return {
    id: "gpt-main",
    provider_id: "openai",
    upstream_model: "gpt-4.1",
    updated_at: 1_700_000_000_000,
    ...overrides,
  };
}

function resetCrud(overrides: Partial<typeof pageState.crud> = {}) {
  pageState.crud = {
    items: [
      model({ id: "beta", provider_id: "anthropic", upstream_model: "claude-3.7" }),
      model({ id: "alpha", provider_id: "openai", upstream_model: "gpt-4.1" }),
    ],
    draft: null,
    loading: false,
    saving: false,
    error: null,
    isEditingExisting: false,
    auxiliaryData: ["anthropic", "openai"],
    setDraft: vi.fn(),
    setError: vi.fn(),
    startEdit: vi.fn(),
    startNew: vi.fn(),
    cancelEdit: vi.fn(),
    handleSave: vi.fn(async () => undefined),
    handleDelete: vi.fn(async () => undefined),
    ...overrides,
  };
  pageState.list = {
    search: "",
    sort: { key: "id", direction: "asc" },
    pageSize: 10,
    page: 1,
    apply: vi.fn(),
  };
}

function renderModels() {
  return render(
    <MemoryRouter initialEntries={["/models"]}>
      <ModelsPage />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  resetCrud();
});

afterEach(() => {
  cleanup();
});

describe("ModelsPage", () => {
  it("renders sorted rows and wires search, page-size, sort, edit, and delete actions", () => {
    renderModels();

    expect(screen.getByText("models.title")).toBeTruthy();
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText("beta")).toBeTruthy();
    expect(screen.getByText("gpt-4.1")).toBeTruthy();

    fireEvent.change(screen.getByPlaceholderText("models.searchPh"), {
      target: { value: "claude" },
    });
    expect(pageState.list.apply).toHaveBeenCalledWith({ search: "claude", page: 1 });

    fireEvent.change(screen.getByDisplayValue("10"), { target: { value: "20" } });
    expect(pageState.list.apply).toHaveBeenCalledWith({ pageSize: 20, page: 1 });

    fireEvent.click(screen.getByRole("button", { name: /Provider/ }));
    expect(pageState.list.apply).toHaveBeenCalledWith({
      sort: { key: "provider_id", direction: "asc" },
      page: 1,
    });

    fireEvent.click(screen.getAllByRole("button", { name: "Edit" })[0]);
    expect(pageState.crud.startEdit).toHaveBeenCalledWith(
      expect.objectContaining({ id: "alpha", provider_id: "openai" }),
    );

    fireEvent.click(screen.getAllByRole("button", { name: "Delete" })[0]);
    expect(pageState.crud.handleDelete).toHaveBeenCalledWith("alpha");
  });

  it("shows pagination and applies corrected page state when the URL page is out of range", async () => {
    pageState.list = {
      ...pageState.list,
      pageSize: 1,
      page: 3,
      apply: vi.fn(),
    };

    renderModels();

    expect(screen.getByText("2/2")).toBeTruthy();
    await waitFor(() => {
      expect(pageState.list.apply).toHaveBeenCalledWith({ page: 2 });
    });

    fireEvent.click(screen.getByRole("button", { name: "‹ Prev" }));
    expect(pageState.list.apply).toHaveBeenCalledWith({ page: 1 });
  });

  it("blocks save for empty drafts and renders all required field errors", () => {
    resetCrud({ draft: model({ id: "", provider_id: "", upstream_model: "" }) });

    renderModels();

    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    const alerts = screen.getAllByRole("alert");
    expect(alerts).toHaveLength(3);
    expect(alerts.every((node) => node.textContent === "validation.required")).toBe(true);
    expect(pageState.crud.handleSave).not.toHaveBeenCalled();
  });

  it("preserves an edited model id, keeps missing providers selectable, and saves valid drafts", () => {
    const draft = model({ id: "legacy-model", provider_id: "legacy", upstream_model: "old-name" });
    resetCrud({
      draft,
      isEditingExisting: true,
      auxiliaryData: ["openai"],
    });

    renderModels();

    expect(screen.getByText("models.formTitle.edit")).toBeTruthy();
    expect((screen.getByDisplayValue("legacy-model") as HTMLInputElement).disabled).toBe(true);
    expect(screen.getByRole("link", { name: "History" }).getAttribute("href")).toContain(
      "models%2Flegacy-model",
    );

    const providerSelect = screen
      .getAllByRole("combobox")
      .find((node) => (node as HTMLSelectElement).value === "legacy") as HTMLSelectElement;
    expect(providerSelect).toBeTruthy();
    expect(Array.from(providerSelect.options).map((option) => option.value)).toEqual([
      "",
      "legacy",
      "openai",
    ]);

    fireEvent.change(providerSelect, { target: { value: "openai" } });
    let updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelBindingSpec | null,
    ) => ModelBindingSpec | null;
    expect(updater(draft)).toEqual({ ...draft, provider_id: "openai" });

    fireEvent.change(screen.getByDisplayValue("old-name"), { target: { value: "gpt-4.1" } });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelBindingSpec | null,
    ) => ModelBindingSpec | null;
    expect(updater(draft)).toEqual({ ...draft, upstream_model: "gpt-4.1" });

    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(pageState.crud.handleSave).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(pageState.crud.cancelEdit).toHaveBeenCalledTimes(1);
  });

  it("renders empty, filtered-empty, and loading list states", () => {
    resetCrud({ items: [] });
    const { rerender, container } = renderModels();

    expect(screen.getByText("models.empty.title")).toBeTruthy();
    fireEvent.click(screen.getAllByRole("button", { name: "models.new" })[0]);
    expect(pageState.crud.startNew).toHaveBeenCalledWith({
      id: "",
      provider_id: "",
      upstream_model: "",
    });

    resetCrud();
    pageState.list = { ...pageState.list, search: "missing" };
    rerender(
      <MemoryRouter initialEntries={["/models"]}>
        <ModelsPage />
      </MemoryRouter>,
    );
    expect(screen.getByText("No models match the current filter.")).toBeTruthy();

    resetCrud({ loading: true });
    rerender(
      <MemoryRouter initialEntries={["/models"]}>
        <ModelsPage />
      </MemoryRouter>,
    );
    expect(container.querySelectorAll(".animate-pulse").length).toBeGreaterThan(0);
  });
});
