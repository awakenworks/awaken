import { useQuery } from "@tanstack/react-query";
import { systemApi, type SystemInfo } from "../../api";
import { qk } from "../keys";

export function useSystemInfoQuery() {
  return useQuery<SystemInfo | null>({
    queryKey: qk.system.info(),
    queryFn: () => systemApi.systemInfo().catch(() => null),
  });
}
