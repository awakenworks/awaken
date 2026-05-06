import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "react-router";
import { router } from "./app";
import { AuthProvider } from "./components/auth-provider";
import { ConfirmDialogProvider } from "./components/confirm-dialog";
import { ToastProvider } from "./components/toast-provider";
import "./lib/i18n";
import { queryClient } from "./lib/query/client";
import "./globals.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <ToastProvider>
        <ConfirmDialogProvider>
          <AuthProvider>
            <RouterProvider router={router} />
          </AuthProvider>
        </ConfirmDialogProvider>
      </ToastProvider>
    </QueryClientProvider>
  </StrictMode>,
);
