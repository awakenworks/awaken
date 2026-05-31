// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { ModelsPage } from "./models-page";
import type { ModelSpec } from "@/lib/config-api";
import type { SortState } from "@/lib/list-view";

const pageState = vi.hoisted(() => ({
  crud: {} as {
    items: ModelSpec[];
    draft: ModelSpec | null;
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
    sort: { key: "id", direction: "asc" } as SortState<
      "id" | "provider_id" | "upstream_model" | "updated_at"
    >,
    pageSize: 10,
    page: 1,
    apply: vi.fn(),
  },
  // Captures the `prepareSave` option ModelsPage passes to useCrudPage so
  // tests can assert payload normalization runs on the mutation closure
  // (not via the racy setDraft → handleSave path).
  capturedOptions: null as { prepareSave?: (draft: ModelSpec) => ModelSpec } | null,
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("@/lib/use-crud-page", () => ({
  useCrudPage: (options: { prepareSave?: (draft: ModelSpec) => ModelSpec }) => {
    pageState.capturedOptions = options;
    return pageState.crud;
  },
}));

vi.mock("@/lib/list-url-state", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/list-url-state")>();
  return {
    ...actual,
    useListUrlState: () => pageState.list,
  };
});

function model(overrides: Partial<ModelSpec>): ModelSpec {
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
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)).toEqual({ ...draft, provider_id: "openai" });

    fireEvent.change(screen.getByDisplayValue("old-name"), { target: { value: "gpt-4.1" } });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
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

  it("rejects max_output_tokens > context_window with an inline error and blocks save", () => {
    resetCrud({
      draft: model({
        id: "m",
        provider_id: "openai",
        upstream_model: "u",
        context_window: 1000,
        max_output_tokens: 2000,
      }),
    });
    renderModels();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("models.validation.maxOutputExceedsContext")).toBeTruthy();
    expect(pageState.crud.handleSave).not.toHaveBeenCalled();
  });

  it("rejects malformed knowledge_cutoff with an inline error", () => {
    resetCrud({
      draft: model({
        id: "m",
        provider_id: "openai",
        upstream_model: "u",
        knowledge_cutoff: "yesterday",
      }),
    });
    renderModels();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("models.validation.knowledgeCutoff")).toBeTruthy();
    expect(pageState.crud.handleSave).not.toHaveBeenCalled();
  });

  it("rejects negative prices with an inline error", () => {
    resetCrud({
      draft: model({
        id: "m",
        provider_id: "openai",
        upstream_model: "u",
        input_token_price_per_million_usd: -1,
      }),
    });
    renderModels();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("models.validation.nonNegativeFinite")).toBeTruthy();
    expect(pageState.crud.handleSave).not.toHaveBeenCalled();
  });

  it("rejects non-integer context_window with an inline error", () => {
    resetCrud({
      draft: model({ id: "m", provider_id: "openai", upstream_model: "u", context_window: 0 }),
    });
    renderModels();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("models.validation.positiveInt")).toBeTruthy();
    expect(pageState.crud.handleSave).not.toHaveBeenCalled();
  });

  it("modality chips enforce set semantics (toggle removes a duplicate rather than adding)", () => {
    const draft = model({
      id: "m",
      provider_id: "openai",
      upstream_model: "u",
      modalities: { input: ["text"] },
    });
    resetCrud({ draft });
    renderModels();

    const textChip = screen.getAllByRole("switch", { name: "text" })[0];
    expect(textChip.getAttribute("aria-checked")).toBe("true");
    expect(textChip.textContent ?? "").toContain("Supported");
    fireEvent.click(textChip);
    const updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.modalities?.input).toEqual([]);

    fireEvent.click(textChip);
    const addUpdater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    // Adding "text" twice still produces a single entry — set semantics preserved.
    const withDuplicateAttempt = addUpdater(
      model({
        id: "m",
        provider_id: "openai",
        upstream_model: "u",
        modalities: { input: ["text"] },
      }),
    );
    expect(withDuplicateAttempt?.modalities?.input).toEqual([]);
  });

  it("renders explicit supported and unsupported modality labels", () => {
    resetCrud({
      draft: model({
        id: "m",
        provider_id: "openai",
        upstream_model: "u",
        modalities: { input: ["text", "image"], output: ["text"] },
      }),
    });
    renderModels();

    expect(screen.getByText("models.fields.modalitiesInputHint")).toBeTruthy();
    expect(screen.getByText(/Supported: text, image/)).toBeTruthy();
    expect(screen.getByText(/Not supported: audio, video, pdf/)).toBeTruthy();
    expect(screen.getAllByText("Not supported").length).toBeGreaterThan(0);
  });

  it("renders capability summary chips for context window, output cap, and modalities", () => {
    resetCrud({
      items: [
        model({
          id: "rich",
          provider_id: "openai",
          upstream_model: "gpt-4.1",
          context_window: 200_000,
          max_output_tokens: 16_384,
          modalities: { input: ["text", "image"], output: ["text"] },
        }),
      ],
    });
    renderModels();
    expect(screen.getByText("200K ctx")).toBeTruthy();
    expect(screen.getByText("16.4K out")).toBeTruthy();
    expect(screen.getByText("in: text/image")).toBeTruthy();
    expect(screen.getByText("out: text")).toBeTruthy();
  });

  it("round-trips all capability fields into setDraft updater calls", () => {
    const draft = model({ id: "m", provider_id: "openai", upstream_model: "u" });
    resetCrud({ draft });
    renderModels();

    fireEvent.change(screen.getByLabelText("models.fields.contextWindow"), {
      target: { value: "200000" },
    });
    let updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.context_window).toBe(200000);

    fireEvent.change(screen.getByLabelText("models.fields.maxOutputTokens"), {
      target: { value: "16384" },
    });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.max_output_tokens).toBe(16384);

    fireEvent.change(screen.getByLabelText("models.fields.knowledgeCutoff"), {
      target: { value: "2026-01" },
    });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.knowledge_cutoff).toBe("2026-01");

    fireEvent.change(screen.getByLabelText(/models\.fields\.inputPrice/), {
      target: { value: "3" },
    });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.input_token_price_per_million_usd).toBe(3);

    fireEvent.change(screen.getByLabelText(/models\.fields\.outputPrice/), {
      target: { value: "15" },
    });
    updater = pageState.crud.setDraft.mock.calls.at(-1)?.[0] as (
      current: ModelSpec | null,
    ) => ModelSpec | null;
    expect(updater(draft)?.output_token_price_per_million_usd).toBe(15);
  });

  it("save payload reflects modality + capability normalization (no setDraft race)", () => {
    renderModels();

    expect(pageState.capturedOptions).not.toBeNull();
    const prepareSave = pageState.capturedOptions!.prepareSave;
    expect(typeof prepareSave).toBe("function");

    // Simulate a dirty draft with duplicate input modalities, blank
    // knowledge_cutoff, and an empty output modality list — every case
    // normalizeModelForSave is supposed to scrub before sending.
    const dirty: ModelSpec = {
      id: "m1",
      provider_id: "openai",
      upstream_model: "gpt-4o",
      knowledge_cutoff: "",
      modalities: { input: ["text", "text", "image"], output: [] },
    };

    const payload = prepareSave!(dirty);

    // Duplicates removed.
    expect(payload.modalities?.input).toEqual(["text", "image"]);
    // Empty output dropped (or normalized to undefined-ish) so the server
    // never sees a meaningless empty array.
    expect(payload.modalities?.output ?? []).toEqual([]);
    // Blank string normalized away (server validator rejects whitespace
    // knowledge_cutoff outright).
    expect(payload.knowledge_cutoff).toBeUndefined();
  });
});
