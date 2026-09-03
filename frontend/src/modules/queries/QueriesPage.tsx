import { useState } from "react";
import { Card, Select, Space, Table, Tag, type TableColumnsType } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import { formatDateTime, formatDuration } from "@/shared/formatters";
import {
  queryOutcomes,
  querySources,
  rcodes,
  transports,
  type QueryParams,
  type QueryRecord,
} from "@/shared/api/types";
import { useQueries } from "./hooks";

const columns: TableColumnsType<QueryRecord> = [
  { title: "时间", dataIndex: "occurred_at", width: 210, render: formatDateTime },
  { title: "耗时", dataIndex: "duration_ms", align: "right", width: 110, render: formatDuration },
  { title: "协议", dataIndex: "transport", width: 90, render: (value: string) => <Tag color="cyan">{value.toUpperCase()}</Tag> },
  { title: "来源", dataIndex: "source", width: 110 },
  { title: "RCODE", dataIndex: "rcode", width: 110 },
  { title: "结果", dataIndex: "outcome", width: 110 },
  { title: "缓存", dataIndex: "cache", width: 100 },
  { title: "策略命中", dataIndex: "policy_matched", width: 100, render: (value: boolean) => (value ? "是" : "否") },
  { title: "资源命中", dataIndex: "resource_matched", width: 100, render: (value: boolean) => (value ? "是" : "否") },
];

export function QueriesPage() {
  const [params, setParams] = useState<QueryParams>({
    page: 1,
    pageSize: 20,
    sort: "occurred_at",
    order: "desc",
  });
  const query = useQueries(params);
  const records = query.data;

  const updateFilter = <K extends keyof QueryParams>(key: K, value: QueryParams[K]) => {
    setParams((current) => ({ ...current, [key]: value, page: 1 }));
  };

  return (
    <PageFrame
      title="解析详情"
      description="展示服务端分页后的安全摘要；不返回或展示原始 qname、客户端标识、digest 与 DNS wire。"
      meta={records ? <SnapshotMeta sampledAt={records.sampled_at} revision={records.runtime_revision} /> : undefined}
    >
      <Card className="data-card table-toolbar">
        <div className="filter-grid">
          <FilterSelect label="协议" value={params.transport} values={transports} onChange={(value) => updateFilter("transport", value)} />
          <FilterSelect label="来源" value={params.source} values={querySources} onChange={(value) => updateFilter("source", value)} />
          <FilterSelect label="RCODE" value={params.rcode} values={rcodes} onChange={(value) => updateFilter("rcode", value)} />
          <FilterSelect label="结果" value={params.outcome} values={queryOutcomes} onChange={(value) => updateFilter("outcome", value)} />
          <div className="filter-field">
            <label>排序字段</label>
            <Select
              value={params.sort}
              options={[{ value: "occurred_at", label: "发生时间" }, { value: "duration_ms", label: "耗时" }]}
              onChange={(value) => updateFilter("sort", value)}
            />
          </div>
          <div className="filter-field">
            <label>排序方向</label>
            <Select
              value={params.order}
              options={[{ value: "desc", label: "降序" }, { value: "asc", label: "升序" }]}
              onChange={(value) => updateFilter("order", value)}
            />
          </div>
        </div>
      </Card>

      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {records ? (
        <Card className="data-card">
          <PageState empty={records.items.length === 0} emptyDescription="当前筛选条件下没有解析摘要" />
          {records.items.length > 0 ? (
            <Table
              rowKey="id"
              columns={columns}
              dataSource={records.items}
              scroll={{ x: 1120 }}
              pagination={{
                current: records.page,
                pageSize: records.page_size,
                total: records.total_items,
                showSizeChanger: true,
                pageSizeOptions: [10, 20, 50, 100],
                onChange: (page, pageSize) => setParams((current) => ({ ...current, page, pageSize })),
              }}
            />
          ) : null}
        </Card>
      ) : null}
    </PageFrame>
  );
}

function FilterSelect<T extends string>({
  label,
  value,
  values,
  onChange,
}: {
  label: string;
  value: T | undefined;
  values: readonly T[];
  onChange: (value: T | undefined) => void;
}) {
  return (
    <div className="filter-field">
      <label>{label}</label>
      <Select
        allowClear
        placeholder="全部"
        value={value}
        options={values.map((item) => ({ value: item, label: item.toUpperCase() }))}
        onChange={onChange}
      />
    </div>
  );
}
