import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { createAdminQueryClient } from "@/lib/query/client";

export function createQueryClientWrapper() {
  const client = createAdminQueryClient();
  return function QueryClientWrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  };
}

export function withQueryClient(children: ReactNode): ReactNode {
  const Wrapper = createQueryClientWrapper();
  return <Wrapper>{children}</Wrapper>;
}
