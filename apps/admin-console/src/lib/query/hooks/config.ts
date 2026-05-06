import { useQuery } from "@tanstack/react-query";
import {
  configResourceApi,
  type ConfigMetaItem,
  type ListResponse,
  type RecordMeta,
} from "../../api";
import { qk } from "../keys";

interface ConfigListQueryOptions {
  enabled?: boolean;
  offset?: number;
  limit?: number;
}

export function useConfigListQuery<T>(namespace: string, options: ConfigListQueryOptions = {}) {
  const offset = options.offset ?? 0;
  const limit = options.limit ?? 100;
  return useQuery<ListResponse<T>>({
    queryKey: qk.config.list(namespace, offset, limit),
    queryFn: () => configResourceApi.list<T>(namespace, offset, limit),
    enabled: options.enabled ?? true,
  });
}

export function useConfigRecordQuery<T>(
  namespace: string,
  id: string | undefined,
  options: { enabled?: boolean } = {},
) {
  return useQuery<T>({
    queryKey: qk.config.get(namespace, id ?? ""),
    queryFn: () => {
      if (!id) throw new Error(`Missing ${namespace} id`);
      return configResourceApi.get<T>(namespace, id);
    },
    enabled: (options.enabled ?? true) && Boolean(id),
  });
}

export function useConfigMetaQuery(
  namespace: string,
  id: string | undefined,
  options: { enabled?: boolean; optional?: boolean } = {},
) {
  return useQuery<RecordMeta | null>({
    queryKey: qk.config.meta(namespace, id ?? ""),
    queryFn: async () => {
      if (!id) throw new Error(`Missing ${namespace} id`);
      try {
        return await configResourceApi.getMeta(namespace, id);
      } catch (error) {
        if (options.optional) return null;
        throw error;
      }
    },
    enabled: (options.enabled ?? true) && Boolean(id),
  });
}

export function useConfigMetaListQuery(namespace: string, options: { enabled?: boolean } = {}) {
  return useQuery<ConfigMetaItem[]>({
    queryKey: qk.config.listMeta(namespace),
    queryFn: () => configResourceApi.listMeta(namespace),
    enabled: options.enabled ?? true,
  });
}
