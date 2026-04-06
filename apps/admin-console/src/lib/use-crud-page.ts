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

export function useCrudPage<TRecord extends { id: string }, TSpec = TRecord>(
  options: CrudPageOptions<TRecord, TSpec>,
): CrudPageState<TRecord> {
  const { namespace, entityLabel, prepareSave, auxiliaryLoaders } = options;

  const [items, setItems] = useState<TRecord[]>([]);
  const [draft, setDraft] = useState<TRecord | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [auxiliaryData, setAuxiliaryData] = useState<unknown[]>([]);

  const isEditingExisting = useMemo(
    () => (draft ? items.some((item) => item.id === draft.id) : false),
    [draft, items],
  );

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const promises: [Promise<{ items: TRecord[] }>, ...Promise<unknown>[]] = [
          configApi.list<TRecord>(namespace),
        ];
        const auxPromise = auxiliaryLoaders?.();
        if (auxPromise) {
          promises.push(auxPromise);
        }

        const results = await Promise.all(promises);
        if (!cancelled) {
          setItems((results[0] as { items: TRecord[] }).items);
          if (results.length > 1) {
            setAuxiliaryData(results[1] as unknown[]);
          }
          setError(null);
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
  }, []);

  const startNew = useCallback((defaults: TRecord) => {
    setDraft({ ...defaults });
  }, []);

  const cancelEdit = useCallback(() => {
    setDraft(null);
  }, []);

  const handleSave = useCallback(async () => {
    if (!draft) {
      return;
    }

    const editing = items.some((item) => item.id === draft.id);
    const payload = prepareSave
      ? prepareSave(draft, editing)
      : (draft as unknown as TSpec);

    setSaving(true);
    try {
      if (editing) {
        const updated = await configApi.update<TSpec, TRecord>(
          namespace,
          draft.id,
          payload,
        );
        setItems((current) =>
          current.map((item) => (item.id === updated.id ? updated : item)),
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
      setError(null);
    } catch (saveError) {
      setError(toErrorMessage(saveError));
    } finally {
      setSaving(false);
    }
    // `draft` and `items` change each render; the callback reads the latest via closure.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft, items, namespace, prepareSave]);

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
