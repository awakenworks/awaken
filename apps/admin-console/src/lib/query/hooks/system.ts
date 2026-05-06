import { useQuery } from "@tanstack/react-query";
import type { SystemInfo } from "../../api";
import { qk } from "../keys";
import { loadOptionalSystemInfo } from "../system-info";

export function useSystemInfoQuery() {
  return useQuery<SystemInfo | null>({
    queryKey: qk.system.info(),
    queryFn: loadOptionalSystemInfo,
  });
}
