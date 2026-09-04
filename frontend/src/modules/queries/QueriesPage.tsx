import { useState, type ReactNode } from "react";
import {
  Alert,
  Card,
  Descriptions,
  Select,
  Space,
  Table,
  Tag,
  Typography,
  type TableColumnsType,
} from "antd";
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

type QueryAnswer = NonNullable<QueryRecord["answers"]>[number];

const answerColumns: TableColumnsType<QueryAnswer> = [
  { title: "类型", dataIndex: "type", width: 90 },
  { title: "名称", dataIndex: "name", width: 240, className: "query-mono" },
  { title: "结果", dataIndex: "data", className: "query-mono query-answer-data" },
  { title: "TTL", dataIndex: "ttl", width: 90, align: "right" },
];

const columns: TableColumnsType<QueryRecord> = [
  {
    title: "时间",
    dataIndex: "occurred_at",
    width: 150,
    responsive: ["sm"],
    render: (value: string) => <TimeCell value={value} />,
  },
  {
    title: "请求",
    key: "request",
    width: 300,
    render: (_, record) => (
      <CellStack
        primary={record.qname ?? "记录产生时未保留域名"}
        secondary={
          <Space size={6} wrap>
            <span>{record.qtype}</span>
            <Tag color="cyan">{record.transport.toUpperCase()}</Tag>
          </Space>
        }
        muted={record.detail_status === "legacy_redacted"}
        mono
      />
    ),
  },
  {
    title: "响应",
    key: "response",
    width: 320,
    render: (_, record) => <ResponseCell record={record} />,
  },
  {
    title: "路由",
    key: "route",
    width: 280,
    responsive: ["md"],
    render: (_, record) => (
      <CellStack
        primary={record.strategy_id ?? (record.detail_status === "legacy_redacted" ? "策略未保留" : "无策略")}
        secondary={formatRoute(record)}
        muted={record.detail_status === "legacy_redacted"}
        mono
      />
    ),
  },
  {
    title: "客户端",
    key: "client",
    width: 210,
    render: (_, record) => {
      const client = formatClient(record);
      return <CellStack primary={client.primary} secondary={client.secondary} muted={client.muted} mono />;
    },
  },
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
      title="解析记录"
      description="查看域名、逻辑响应、实际路由与有效客户端；查询详情仅向已认证 WebUI 用户提供。"
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
        <Card className="data-card query-record-card">
          <PageState empty={records.items.length === 0} emptyDescription="当前筛选条件下没有解析记录" />
          {records.items.length > 0 ? (
            <Table
              rowKey="id"
              columns={columns}
              dataSource={records.items}
              scroll={{ x: 860 }}
              expandable={{ expandedRowRender: (record) => <QueryDetails record={record} /> }}
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

function TimeCell({ value }: { value: string }) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return <Typography.Text type="secondary">—</Typography.Text>;
  }
  return (
    <CellStack
      primary={date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}
      secondary={date.toLocaleDateString()}
    />
  );
}

function ResponseCell({ record }: { record: QueryRecord }) {
  const answer = record.answers?.[0];
  const summary = formatResponseSummary(record);
  return (
    <CellStack
      primary={summary.primary}
      secondary={
        <Space size={6} wrap>
          <span>{formatDuration(record.duration_ms)}</span>
          <span>{summary.meta}</span>
          <Tag color={record.source === "cache" ? "green" : "blue"}>{sourceLabel(record)}</Tag>
        </Space>
      }
      mono={Boolean(answer)}
    />
  );
}

function QueryDetails({ record }: { record: QueryRecord }) {
  const legacy = record.detail_status === "legacy_redacted";
  return (
    <Space direction="vertical" size={16} className="query-details">
      {legacy ? <Alert type="info" showIcon message="该记录产生时未保留域名、客户端、路由和响应详情" /> : null}
      <Descriptions bordered size="small" column={{ xs: 1, sm: 2, lg: 3 }}>
        <Descriptions.Item label="请求">{record.qname ?? "未保留"}</Descriptions.Item>
        <Descriptions.Item label="类型 / 协议">{record.qtype} / {record.transport.toUpperCase()}</Descriptions.Item>
        <Descriptions.Item label="发生时间">{formatDateTime(record.occurred_at)}</Descriptions.Item>
        <Descriptions.Item label="响应">{record.rcode} / {record.outcome}</Descriptions.Item>
        <Descriptions.Item label="来源 / 缓存">{record.source} / {record.cache}</Descriptions.Item>
        <Descriptions.Item label="耗时">{formatDuration(record.duration_ms)}</Descriptions.Item>
        <Descriptions.Item label="当前 strategy">{record.strategy_id ?? "无"}</Descriptions.Item>
        <Descriptions.Item label="upstream target">{record.upstream_target_id ?? "无"}</Descriptions.Item>
        <Descriptions.Item label="actual upstream">{record.upstream_used_id ?? "未确定"}</Descriptions.Item>
        <Descriptions.Item label="客户端名称">{record.client_name ?? "未命中配置"}</Descriptions.Item>
        <Descriptions.Item label="有效客户端 IP">{record.client_ip ?? "未知"}</Descriptions.Item>
        <Descriptions.Item label="记录 ID">{record.id}</Descriptions.Item>
        <Descriptions.Item label="命中摘要">
          strategy/rule {record.policy_matched ? "已命中" : "未命中"}，资源 {record.resource_matched ? "已命中" : "未命中"}
        </Descriptions.Item>
      </Descriptions>
      {record.source === "cache" && !legacy ? (
        <Alert type="info" showIcon message="upstream 信息来自缓存生产请求；本次解析未调用该 upstream" />
      ) : null}
      {record.answers ? (
        <div>
          <Typography.Title level={5}>响应 Answer</Typography.Title>
          {record.answers_truncated ? (
            <Alert
              className="query-answer-alert"
              type="warning"
              showIcon
              message={`仅保留前 ${record.answers.length} 条，共 ${record.answer_count ?? record.answers.length} 条`}
            />
          ) : null}
          <Table
            rowKey={(_, index) => String(index)}
            columns={answerColumns}
            dataSource={record.answers}
            locale={{ emptyText: "响应没有 answer 记录" }}
            pagination={false}
            size="small"
            scroll={{ x: 700 }}
          />
        </div>
      ) : null}
    </Space>
  );
}

export function formatRoute(record: QueryRecord): string {
  if (record.detail_status === "legacy_redacted") {
    return "记录产生时未保留路由";
  }
  if (record.source === "hosts" || record.source === "rule" || record.source === "synthetic") {
    return "本地响应";
  }
  const route = record.upstream_target_id
    ? record.upstream_used_id && record.upstream_used_id !== record.upstream_target_id
      ? `${record.upstream_target_id} → ${record.upstream_used_id}`
      : record.upstream_used_id ?? `${record.upstream_target_id} → 未确定`
    : "upstream 未确定";
  return record.source === "cache" ? `缓存来源：${route}` : route;
}

export function formatClient(record: QueryRecord): { primary: string; secondary?: string; muted: boolean } {
  return {
    primary: record.client_name ?? record.client_ip ?? "未知客户端",
    secondary: record.client_name && record.client_ip ? record.client_ip : undefined,
    muted: !record.client_name && !record.client_ip,
  };
}

export function formatResponseSummary(record: QueryRecord): { primary: string; meta: string } {
  const answer = record.answers?.[0];
  const retained = record.answers?.length ?? 0;
  return {
    primary: answer ? `${answer.type}  ${answer.data}` : `${record.rcode} · ${record.outcome}`,
    meta: record.answer_count === null
      ? "结果未保留"
      : record.answers_truncated
        ? `仅保留 ${retained} 条，共 ${record.answer_count} 条`
        : `${record.answer_count} 条结果`,
  };
}

function sourceLabel(record: QueryRecord): string {
  if (record.source === "cache") {
    return record.cache === "stale" ? "过期缓存" : "缓存命中";
  }
  return record.source;
}

function CellStack({
  primary,
  secondary,
  muted = false,
  mono = false,
}: {
  primary: ReactNode;
  secondary?: ReactNode;
  muted?: boolean;
  mono?: boolean;
}) {
  return (
    <div className={`query-cell${mono ? " query-mono" : ""}`}>
      <Typography.Text type={muted ? "secondary" : undefined}>{primary}</Typography.Text>
      {secondary ? <Typography.Text type="secondary" className="query-cell-secondary">{secondary}</Typography.Text> : null}
    </div>
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
