import { useCallback, useEffect, useState } from "react";
import { ConfigApiError, configApi } from "@/lib/config-api";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { UsedByList } from "@/components/used-by-list";

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

function capitalize(value: string): string {
  if (value.length === 0) return value;
  return value[0].toUpperCase() + value.slice(1);
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
  const toast = useToast();
  const confirmDialog = useConfirmDialog();

  const [items, setItems] = useState<TRecord[]>([]);
  const [draft, setDraft] = useState<TRecord | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [auxiliaryData, setAuxiliaryData] = useState<unknown[]>([]);

  const isEditingExisting = editingId !== null;

  const reportError = useCallback(
    (message: string) => {
      setError(message);
      toast.error(message);
    },
    [toast],
  );

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const result = await loadCrudPageData<TRecord>(namespace, auxiliaryLoaders);
        if (!cancelled) {
          setItems(result.items);
          setAuxiliaryData(result.auxiliaryData);
          if (result.auxiliaryError) {
            reportError(result.auxiliaryError);
          } else {
            setError(null);
          }
        }
      } catch (loadError) {
        if (!cancelled) {
          reportError(toErrorMessage(loadError));
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
        toast.success(`${capitalize(entityLabel)} "${editingId}" saved`);
      } else {
        const created = await configApi.create<TSpec, TRecord>(namespace, payload);
        setItems((current) =>
          [...current.filter((item) => item.id !== created.id), created].sort(
            (left, right) => left.id.localeCompare(right.id),
          ),
        );
        toast.success(`${capitalize(entityLabel)} "${created.id}" created`);
      }
      setDraft(null);
      setEditingId(null);
      setError(null);
    } catch (saveError) {
      reportError(toErrorMessage(saveError));
    } finally {
      setSaving(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft, editingId, namespace, prepareSave, entityLabel, toast, reportError]);

  const handleDelete = useCallback(
    async (id: string) => {
      const confirmed = await confirmDialog({
        title: `Delete ${entityLabel}?`,
        description: (
          <>
            This permanently removes <span className="font-mono">{id}</span> from
            the runtime catalog.
          </>
        ),
        confirmLabel: "Delete",
        tone: "destructive",
      });
      if (!confirmed) {
        return;
      }

      try {
        await configApi.delete(namespace, id);
        setItems((current) => current.filter((item) => item.id !== id));
        setError(null);
        toast.success(`${capitalize(entityLabel)} "${id}" deleted`);
      } catch (deleteError) {
        if (
          deleteError instanceof ConfigApiError &&
          deleteError.status === 409 &&
          deleteError.detail !== null &&
          typeof deleteError.detail === "object" &&
          "used_by" in deleteError.detail &&
          Array.isArray((deleteError.detail as Record<string, unknown>).used_by)
        ) {
          const usedBy = (
            deleteError.detail as { used_by: Array<{ namespace: string; id: string }> }
          ).used_by;
          const force = await confirmDialog({
            title: "Still delete?",
            description: <UsedByList items={usedBy} />,
            confirmLabel: "Force delete",
            tone: "destructive",
          });
          if (!force) return;
          try {
            await configApi.delete(namespace, id, { force: true });
            setItems((current) => current.filter((item) => item.id !== id));
            setError(null);
            toast.success(`${capitalize(entityLabel)} "${id}" deleted`);
          } catch (forceError) {
            reportError(toErrorMessage(forceError));
          }
        } else {
          reportError(toErrorMessage(deleteError));
        }
      }
    },
    [namespace, entityLabel, confirmDialog, toast, reportError],
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
