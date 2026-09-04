import { type ReactNode } from "react";
export type ToastTone = "success" | "danger" | "error" | "info" | "warning";
export type ToastAction = {
    readonly label: string;
    readonly onClick: () => void;
};
export interface ToastRequest {
    readonly message: ReactNode;
    readonly tone?: ToastTone;
    readonly duration?: number;
    readonly action?: ToastAction;
}
export interface ToastApi {
    readonly push: (request: ToastRequest) => number;
    readonly dismiss: (id: number) => void;
}
export declare function useToast({ optional }?: {
    readonly optional?: boolean;
}): ToastApi;
export type ToastProviderProps = {
    readonly children: ReactNode;
    readonly defaultDuration?: number;
    readonly errorDuration?: number;
    readonly dismissLabel: string;
    readonly regionLabel: string;
    readonly renderIcon?: (tone: ToastTone) => ReactNode;
};
export declare function ToastProvider({ children, defaultDuration, errorDuration, dismissLabel, regionLabel, renderIcon, }: ToastProviderProps): import("react/jsx-runtime").JSX.Element;
