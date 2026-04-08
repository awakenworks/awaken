import { useCallback, useEffect, useMemo, useState } from "react";
import { configApi } from "@/lib/config-api";

export interface CrudPageOptions<TRecord extends { id: string }, TSpec = TRecord> {
  /** API namespace used for list/create/update/delete calls. */
  namespace: string;
  /** Confirmation label shown in the delete dialog (e.g. "model", "provider"). */
  entityLabel: string;
  /**
   * Transform the draft record into the spec payload sent to the API.
   * When omitted the draft is sent as-is (cast to `TSpec`).
   */
  prepareSave?: (draft: TRecord, isEditing: boolean) => TSpec;
  /**
   * Additional async loaders that run in parallel with the main list fetch.
   * Results are returned via the `auxiliaryData` field on the hook return value.
   */
  auxiliaryLoaders?: () => Promise<unknown[]>;
}

export interface CrudPageState<TRecord extends { id: string }> {
  items: TRecord[];
  draft: TRecord | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
  isEditingExisting: boolean;
  setDraft: React.Dispatch<React.SetStateAction<TRecord | null>>;
  setError: React.Dispatch<React.SetStateAction<string | null>>;
  startEdit: (item: TRecord) => void;
  startNew: (defaults: TRecord) => void;
  cancelEdit: () => void;
  handleSave: () => Promise<void>;
  handleDelete: (id: string) => Promise<void>;
  auxiliaryData: unknown[];
}

function toErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export interface CrudPageLoadResult<TRecord extends { id: string }> {
  items: TRecord[];
  auxiliaryData: unknown[];
  auxiliaryError: string | null;
}

export async function loadCrudPageData<TRecord extends { id: string }>(
  namespace: string,
  auxiliaryLoaders?: () => Promise<unknown[]>,
): Promise<CrudPageLoadResult<TRecord>> {
  const listPromise = configApi.list<TRecord>(namespace);
  const auxiliaryPromise = auxiliaryLoaders?.();
  const [listResult, auxiliaryResult] = await Promise.allSettled([
    listPromise,
    auxiliaryPromise ?? Promise.resolve(undefined),
  ] as const);

  if (listResult.status === "rejected") {
    throw listResult.reason;
  }

  if (!auxiliaryPromise) {
    return {
      items: listResult.value.items,
      auxiliaryData: [],
      auxiliaryError: null,
    };
  }

  if (auxiliaryResult.status === "rejected") {
    return {
      items: listResult.value.items,
      auxiliaryData: [],
      auxiliaryError: toErrorMessage(auxiliaryResult.reason),
    };
  }

  return {
    items: listResult.value.items,
    auxiliaryData: auxiliaryResult.value ?? [],
    auxiliaryError: null,
  };
}

export function useCrudPage<TRecord extends { id: string }, TSpec = TRecord>(
  options: CrudPageOptions<TRecord, TSpec>,
): CrudPageState<TRecord> {
  const { namespace, entityLabel, prepareSave, auxiliaryLoaders } = options;

  const [items, setItems] = useState<TRecord[]>([]);
  const [draft, setDraft] = useState<TRecord | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [auxiliaryData, setAuxiliaryData] = useState<unknown[]>([]);

  const isEditingExisting = editingId !== null;

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const result = await loadCrudPageData<TRecord>(namespace, auxiliaryLoaders);
        if (!cancelled) {
          setItems(result.items);
          setAuxiliaryData(result.auxiliaryData);
          setError(result.auxiliaryError);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(toErrorMessage(loadError));
          setItems([]);
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    }

    void load();

    return () => {
      cancelled = true;
    };
    // Options are expected to be stable across renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startEdit = useCallback((item: TRecord) => {
    setDraft({ ...item });
    setEditingId(item.id);
  }, []);

  const startNew = useCallback((defaults: TRecord) => {
    setDraft({ ...defaults });
    setEditingId(null);
  }, []);

  const cancelEdit = useCallback(() => {
    setDraft(null);
    setEditingId(null);
  }, []);

  const handleSave = useCallback(async () => {
    if (!draft) {
      return;
    }

    const editing = editingId !== null;
    const payload = prepareSave
      ? prepareSave(draft, editing)
      : (draft as unknown as TSpec);

    setSaving(true);
    try {
      if (editing) {
        const updated = await configApi.update<TSpec, TRecord>(
          namespace,
          editingId,
          payload,
        );
        setItems((current) =>
          current.map((item) => (item.id === editingId ? updated : item)),
        );
      } else {
        const created = await configApi.create<TSpec, TRecord>(namespace, payload);
        setItems((current) =>
          [...current.filter((item) => item.id !== created.id), created].sort(
            (left, right) => left.id.localeCompare(right.id),
          ),
        );
      }
      setDraft(null);
      setEditingId(null);
      setError(null);
    } catch (saveError) {
      setError(toErrorMessage(saveError));
    } finally {
      setSaving(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft, editingId, namespace, prepareSave]);

  const handleDelete = useCallback(
    async (id: string) => {
      if (!confirm(`Delete ${entityLabel} "${id}"?`)) {
        return;
      }

      try {
        await configApi.delete(namespace, id);
        setItems((current) => current.filter((item) => item.id !== id));
        setError(null);
      } catch (deleteError) {
        setError(toErrorMessage(deleteError));
      }
    },
    [namespace, entityLabel],
  );

  return {
    items,
    draft,
    loading,
    saving,
    error,
    isEditingExisting,
    setDraft,
    setError,
    startEdit,
    startNew,
    cancelEdit,
    handleSave,
    handleDelete,
    auxiliaryData,
  };
}
