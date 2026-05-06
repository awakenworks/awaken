import { useCallback, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ConfigApiError, configResourceApi } from "@/lib/api";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { UsedByList } from "@/components/used-by-list";
import { qk } from "@/lib/query/keys";
import { invalidateConfigMutation, removeConfigResourceQueries } from "@/lib/query/invalidation";

interface CrudPageBaseOptions<TRecord extends { id: string }, TSpec = TRecord> {
  /** API namespace used for list/create/update/delete calls. */
  namespace: string;
  /** Confirmation label shown in the delete dialog (e.g. "model", "provider"). */
  entityLabel: string;
  /**
   * Transform the draft record into the spec payload sent to the API.
   * When omitted the draft is sent as-is (cast to `TSpec`).
   */
  prepareSave?: (draft: TRecord, isEditing: boolean) => TSpec;
}

interface CrudPageAuxiliaryOptions {
  /**
   * Additional async loaders that run in parallel with the main list fetch.
   * Results are returned via the `auxiliaryData` field on the hook return value.
   */
  auxiliaryLoaders: () => Promise<unknown[]>;
  /** Stable query-key fragment that identifies this auxiliary loader set. */
  auxiliaryQueryKey: readonly unknown[];
}

interface CrudPageWithoutAuxiliaryOptions {
  auxiliaryLoaders?: undefined;
  auxiliaryQueryKey?: undefined;
}

export type CrudPageOptions<TRecord extends { id: string }, TSpec = TRecord> = CrudPageBaseOptions<
  TRecord,
  TSpec
> &
  (CrudPageAuxiliaryOptions | CrudPageWithoutAuxiliaryOptions);

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

const EMPTY_ITEMS: never[] = [];
const EMPTY_AUXILIARY: unknown[] = [];

export async function loadCrudPageData<TRecord extends { id: string }>(
  namespace: string,
  auxiliaryLoaders?: () => Promise<unknown[]>,
): Promise<CrudPageLoadResult<TRecord>> {
  const listPromise = configResourceApi.list<TRecord>(namespace);
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
  const { namespace, entityLabel, prepareSave, auxiliaryLoaders, auxiliaryQueryKey } = options;
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const queryClient = useQueryClient();
  const queryKey = useMemo(
    () =>
      auxiliaryLoaders
        ? qk.config.listWithAux(namespace, auxiliaryQueryKey)
        : qk.config.list(namespace),
    [auxiliaryLoaders, auxiliaryQueryKey, namespace],
  );

  const [draft, setDraft] = useState<TRecord | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const isEditingExisting = editingId !== null;
  const query = useQuery<CrudPageLoadResult<TRecord>>({
    queryKey,
    queryFn: () => loadCrudPageData<TRecord>(namespace, auxiliaryLoaders),
  });

  const items = query.data?.items ?? EMPTY_ITEMS;
  const auxiliaryData = query.data?.auxiliaryData ?? EMPTY_AUXILIARY;

  const reportError = useCallback(
    (message: string) => {
      setError(message);
      toast.error(message);
    },
    [toast],
  );

  const loadErrorMessage = query.error
    ? toErrorMessage(query.error)
    : (query.data?.auxiliaryError ?? null);

  useEffect(() => {
    if (loadErrorMessage) {
      reportError(loadErrorMessage);
      return;
    }
    setError(null);
  }, [loadErrorMessage, reportError]);

  const updateCachedItems = useCallback(
    (updater: (current: TRecord[]) => TRecord[]) => {
      queryClient.setQueryData<CrudPageLoadResult<TRecord>>(queryKey, (current) => ({
        items: updater(current?.items ?? []),
        auxiliaryData: current?.auxiliaryData ?? [],
        auxiliaryError: current?.auxiliaryError ?? null,
      }));
    },
    [queryClient, queryKey],
  );

  const { mutateAsync: saveRecord, isPending: saving } = useMutation({
    mutationFn: async ({
      currentDraft,
      currentEditingId,
    }: {
      currentDraft: TRecord;
      currentEditingId: string | null;
    }) => {
      const editing = currentEditingId !== null;
      const payload = prepareSave
        ? prepareSave(currentDraft, editing)
        : (currentDraft as unknown as TSpec);
      if (editing) {
        return {
          editingId: currentEditingId,
          record: await configResourceApi.update<TSpec, TRecord>(
            namespace,
            currentEditingId,
            payload,
          ),
        };
      }
      return {
        editingId: null,
        record: await configResourceApi.create<TSpec, TRecord>(namespace, payload),
      };
    },
  });

  const { mutateAsync: deleteRecord } = useMutation({
    mutationFn: ({ id, force }: { id: string; force?: boolean }) => {
      if (force) return configResourceApi.delete(namespace, id, { force: true });
      return configResourceApi.delete(namespace, id);
    },
  });

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

    try {
      const result = await saveRecord({
        currentDraft: draft,
        currentEditingId: editingId,
      });
      if (result.editingId) {
        updateCachedItems((current) =>
          current.map((item) => (item.id === result.editingId ? result.record : item)),
        );
        invalidateConfigMutation(queryClient, namespace, result.editingId);
        toast.success(`${capitalize(entityLabel)} "${result.editingId}" saved`);
      } else {
        updateCachedItems((current) =>
          [...current.filter((item) => item.id !== result.record.id), result.record].sort(
            (left, right) => left.id.localeCompare(right.id),
          ),
        );
        invalidateConfigMutation(queryClient, namespace, result.record.id);
        toast.success(`${capitalize(entityLabel)} "${result.record.id}" created`);
      }
      setDraft(null);
      setEditingId(null);
      setError(null);
    } catch (saveError) {
      reportError(toErrorMessage(saveError));
    }
  }, [
    draft,
    editingId,
    entityLabel,
    namespace,
    queryClient,
    reportError,
    saveRecord,
    toast,
    updateCachedItems,
  ]);

  const handleDelete = useCallback(
    async (id: string) => {
      const confirmed = await confirmDialog({
        title: `Delete ${entityLabel}?`,
        description: (
          <>
            This permanently removes <span className="font-mono">{id}</span> from the runtime
            catalog.
          </>
        ),
        confirmLabel: "Delete",
        tone: "destructive",
      });
      if (!confirmed) {
        return;
      }

      try {
        await deleteRecord({ id });
        updateCachedItems((current) => current.filter((item) => item.id !== id));
        removeConfigResourceQueries(queryClient, namespace, id);
        invalidateConfigMutation(queryClient, namespace, id);
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
            await deleteRecord({ id, force: true });
            updateCachedItems((current) => current.filter((item) => item.id !== id));
            removeConfigResourceQueries(queryClient, namespace, id);
            invalidateConfigMutation(queryClient, namespace, id);
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
    [
      confirmDialog,
      deleteRecord,
      entityLabel,
      namespace,
      queryClient,
      reportError,
      toast,
      updateCachedItems,
    ],
  );

  return {
    items,
    draft,
    loading: query.isPending,
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
