import {
  DataGrid as SharedDataGrid,
  type DataGridColumn,
} from "@awaken/ui";
import type { ReactNode } from "react";
import { useApp } from "../../lib/app-state";
import type { ListState } from "../../lib/useListState";
import { EmptyState, Skeleton, SkeletonRows } from "./primitives";

export type Column<Row> = DataGridColumn<Row>;

export interface DataGridProps<Row> {
  readonly rows: Row[];
  readonly columns: Column<Row>[];
  readonly rowKey: (row: Row) => string;
  readonly state: ListState;
  readonly filter?: (row: Row, query: string) => boolean;
  readonly onRowClick?: (row: Row) => void;
  readonly pageSize?: number;
  readonly loading?: boolean;
  readonly emptyTitle?: string;
  readonly emptyHint?: string;
  readonly toolbar?: ReactNode;
  readonly searchPlaceholder?: string;
}

export function DataGrid<Row>({
  emptyTitle,
  emptyHint,
  searchPlaceholder,
  ...props
}: DataGridProps<Row>) {
  const app = useApp();
  return (
    <SharedDataGrid
      {...props}
      classes={{
        button: "btn ghost",
        muted: "mut",
        pager: "row",
        range: "mut",
        scroll: "card grid-scroll",
        search: "input",
        table: "table",
        toolbar: "row",
        toolbarLead: "row",
      }}
      labels={{
        formatRange: ({ from, to, total }) =>
          app.t(`${from}–${to} of ${total}`, `${from}–${to} / ${total}`),
        next: <>{app.t("Next", "下一页")} →</>,
        previous: <>← {app.t("Prev", "上一页")}</>,
        searchPlaceholder: searchPlaceholder ?? app.t("Filter…", "过滤…"),
      }}
      mobileCards
      mobileRowActionLabel={app.t("Open", "打开")}
      renderEmpty={() => (
        <EmptyState
          hint={emptyHint}
          title={emptyTitle ?? app.t("Nothing here yet.", "暂无数据。")}
        />
      )}
      renderLoading={(columns) => <SkeletonRows cols={columns} rows={4} />}
      renderMobileLoading={() => <div className="card" style={{ padding: 12 }}><Skeleton height={48} /></div>}
    />
  );
}
