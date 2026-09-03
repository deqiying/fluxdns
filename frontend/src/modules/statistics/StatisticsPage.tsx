import { useMemo, useState } from "react";
import { Alert, Button, Card, Input, Select, Space, Table, Tag, type TableColumnsType } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import { formatCount } from "@/shared/formatters";
import { statisticDimensions, type StatisticItem, type StatisticsParams } from "@/shared/api/types";
import { useStatistics } from "./hooks";

const dimensionLabels: Record<string, string> = {
  total: "总量",
  transport: "协议",
  source: "来源",
  rcode: "RCODE",
  outcome: "结果",
  cache: "缓存",
};

function defaultDates() {
  const to = new Date();
  const from = new Date(to);
  from.setUTCDate(from.getUTCDate() - 6);
  return { dateFrom: from.toISOString().slice(0, 10), dateTo: to.toISOString().slice(0, 10) };
}

const initialDates = defaultDates();

const columns: TableColumnsType<StatisticItem> = [
  { title: "UTC 日期", dataIndex: "date", width: 140 },
  {
    title: "维度",
    dataIndex: "dimension_kind",
    width: 130,
    render: (value: string) => <Tag>{dimensionLabels[value] ?? value}</Tag>,
  },
  { title: "值", dataIndex: "dimension_value" },
  { title: "数量", dataIndex: "count", align: "right", width: 160, render: formatCount },
];

export function StatisticsPage() {
  const [draft, setDraft] = useState(() => ({ ...initialDates, dimension: "total" as const }));
  const [params, setParams] = useState<StatisticsParams>({
    ...initialDates,
    dimension: "total",
    page: 1,
    pageSize: 20,
  });
  const [validationError, setValidationError] = useState<string>();
  const query = useStatistics(params);
  const statistics = query.data;

  const rangeDays = useMemo(() => {
    const from = Date.parse(`${draft.dateFrom}T00:00:00Z`);
    const to = Date.parse(`${draft.dateTo}T00:00:00Z`);
    return Number.isFinite(from) && Number.isFinite(to) ? Math.floor((to - from) / 86_400_000) + 1 : 0;
  }, [draft.dateFrom, draft.dateTo]);

  const applyFilters = () => {
    if (rangeDays < 1) {
      setValidationError("结束日期不能早于起始日期。");
      return;
    }
    if (rangeDays > 31) {
      setValidationError("统计区间不能超过 31 天。");
      return;
    }
    setValidationError(undefined);
    setParams((current) => ({ ...current, ...draft, page: 1 }));
  };

  return (
    <PageFrame
      title="解析统计"
      description="按 UTC 日期和有限维度读取服务端聚合结果；浏览器不加载明细后再聚合。"
      meta={statistics ? <SnapshotMeta sampledAt={statistics.sampled_at} revision={statistics.runtime_revision} /> : undefined}
    >
      <Card className="data-card table-toolbar">
        <div className="filter-grid">
          <div className="filter-field">
            <label htmlFor="statistics-date-from">起始日期（UTC）</label>
            <Input id="statistics-date-from" type="date" value={draft.dateFrom} onChange={(event) => setDraft({ ...draft, dateFrom: event.target.value })} />
          </div>
          <div className="filter-field">
            <label htmlFor="statistics-date-to">结束日期（UTC）</label>
            <Input id="statistics-date-to" type="date" value={draft.dateTo} onChange={(event) => setDraft({ ...draft, dateTo: event.target.value })} />
          </div>
          <div className="filter-field">
            <label htmlFor="statistics-dimension">聚合维度</label>
            <Select
              id="statistics-dimension"
              value={draft.dimension}
              options={statisticDimensions.map((value) => ({ value, label: dimensionLabels[value] }))}
              onChange={(dimension) => setDraft({ ...draft, dimension })}
            />
          </div>
          <Space align="end">
            <Button type="primary" onClick={applyFilters}>查询</Button>
          </Space>
        </div>
        {validationError ? <Alert type="warning" showIcon message={validationError} style={{ marginTop: 14 }} /> : null}
      </Card>

      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {statistics ? (
        <Card className="data-card">
          <PageState empty={statistics.items.length === 0} emptyDescription="当前条件下没有聚合结果" />
          {statistics.items.length > 0 ? (
            <Table
              rowKey={(item) => `${item.date}:${item.dimension_kind}:${item.dimension_value}`}
              columns={columns}
              dataSource={statistics.items}
              scroll={{ x: 680 }}
              pagination={{
                current: statistics.page,
                pageSize: statistics.page_size,
                total: statistics.total_items,
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
