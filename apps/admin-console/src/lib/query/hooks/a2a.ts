import { useQuery } from "@tanstack/react-query";
import { a2aApi, ConfigApiError, type A2aServerStatusResponse } from "../../api";
import { qk } from "../keys";

export function useA2aStatusQuery(id: string | undefined) {
  return useQuery<A2aServerStatusResponse | null>({
    queryKey: qk.a2a.status(id ?? ""),
    queryFn: async () => {
      if (!id) throw new Error("Missing A2A server id");
      try {
        return await a2aApi.a2aStatus(id);
      } catch (error) {
        if (error instanceof ConfigApiError && error.status === 404) {
          return null;
        }
        throw error;
      }
    },
    enabled: Boolean(id),
    staleTime: 15_000,
    refetchOnWindowFocus: false,
  });
}
