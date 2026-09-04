import type { ReactNode } from "react";
export interface DataGridColumn<Row> {
    readonly key: string;
    readonly header: ReactNode;
    readonly cell: (row: Row) => ReactNode;
    readonly sortValue?: (row: Row) => string | number;
    readonly align?: "left" | "right";
    readonly width?: number | string;
}
export interface DataGridState {
    readonly q: string;
    readonly sort: string;
    readonly dir: "asc" | "desc";
    readonly page: number;
    readonly setQ: (value: string) => void;
    readonly setSort: (key: string) => void;
    readonly setPage: (page: number) => void;
}
export interface DataGridLabels {
    readonly searchPlaceholder: string;
    readonly previous: ReactNode;
    readonly next: ReactNode;
    readonly formatRange: (range: {
        readonly from: number;
        readonly to: number;
        readonly total: number;
    }) => ReactNode;
}
export interface DataGridClasses {
    readonly toolbar?: string;
    readonly toolbarLead?: string;
    readonly search?: string;
    readonly range?: string;
    readonly scroll?: string;
    readonly table?: string;
    readonly muted?: string;
    readonly pager?: string;
    readonly button?: string;
}
export interface DataGridProps<Row> {
    readonly rows: readonly Row[];
    readonly columns: readonly DataGridColumn<Row>[];
    readonly rowKey: (row: Row) => string;
    readonly state: DataGridState;
    readonly labels: DataGridLabels;
    readonly filter?: (row: Row, query: string) => boolean;
    readonly onRowClick?: (row: Row) => void;
    readonly pageSize?: number;
    readonly loading?: boolean;
    readonly toolbar?: ReactNode;
    /** Render a label-value card view below 760px while retaining the native table
     * for wider viewports. Products opt in after checking their cell content. */
    readonly mobileCards?: boolean;
    /** Accessible label for the per-card action when `onRowClick` is present. */
    readonly mobileRowActionLabel?: ReactNode | ((row: Row) => ReactNode);
    readonly renderMobileLoading?: () => ReactNode;
    readonly renderLoading: (columns: number) => ReactNode;
    readonly renderEmpty: () => ReactNode;
    readonly classes?: DataGridClasses;
}
export declare function DataGrid<Row>({ rows, columns, rowKey, state, labels, filter, onRowClick, pageSize, loading, toolbar, mobileCards, mobileRowActionLabel, renderMobileLoading, renderLoading, renderEmpty, classes, }: DataGridProps<Row>): import("react/jsx-runtime").JSX.Element;
