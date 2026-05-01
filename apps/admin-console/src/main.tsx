import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { RouterProvider } from "react-router";
import { router } from "./app";
import { AuthProvider } from "./components/auth-provider";
import { ConfirmDialogProvider } from "./components/confirm-dialog";
import { ToastProvider } from "./components/toast-provider";
import "./globals.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ToastProvider>
      <ConfirmDialogProvider>
        <AuthProvider>
          <RouterProvider router={router} />
        </AuthProvider>
      </ConfirmDialogProvider>
    </ToastProvider>
  </StrictMode>,
);
