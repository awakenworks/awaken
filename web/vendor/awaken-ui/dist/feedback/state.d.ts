import type { ReactNode } from "react";
export type StateAction = {
    readonly label: string;
    readonly onClick?: () => void;
    readonly href?: string;
};
export type StateProps = {
    readonly action?: StateAction;
    readonly actions?: ReactNode;
    readonly body?: ReactNode;
    readonly className?: string;
    readonly icon?: ReactNode;
    readonly title: ReactNode;
};
export declare function EmptyState(props: StateProps): import("react/jsx-runtime").JSX.Element;
export declare function ErrorState(props: StateProps): import("react/jsx-runtime").JSX.Element;
export declare function LoadingState({ label, icon }: {
    readonly label: ReactNode;
    readonly icon?: ReactNode;
}): import("react/jsx-runtime").JSX.Element;
export declare function LoadingRow({ label, icon }: {
    readonly label: ReactNode;
    readonly icon?: ReactNode;
}): import("react/jsx-runtime").JSX.Element;
export declare function SkeletonList({ rows, label }: {
    readonly rows?: number;
    readonly label: string;
}): import("react/jsx-runtime").JSX.Element;
export declare function Skeleton({ width, height, className, }: {
    readonly width?: number | string;
    readonly height?: number;
    readonly className?: string;
}): import("react/jsx-runtime").JSX.Element;
export type GateQuery = {
    readonly isLoading: boolean;
    readonly isError: boolean;
    readonly refetch?: () => unknown;
};
export type SurfaceGateProps = {
    readonly query: GateQuery;
    readonly isEmpty?: boolean;
    readonly loading: string;
    readonly loadingContent?: ReactNode;
    readonly error: {
        readonly title: string;
        readonly body: string;
        readonly retry: string;
    };
    readonly empty?: StateProps;
    readonly loadingIcon?: ReactNode;
    readonly errorIcon?: ReactNode;
    readonly emptyIcon?: ReactNode;
    readonly children: ReactNode;
};
export declare function SurfaceGate({ query, isEmpty, loading, loadingContent, error, empty, loadingIcon, errorIcon, emptyIcon, children, }: SurfaceGateProps): ReactNode;
