import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Alert,
  Button,
  Descriptions,
  Input,
  Popover,
  Segmented,
  Select,
  Space,
  Switch,
  Table,
  Tag,
  Typography,
  type TableColumnsType,
} from "antd";
import { ChevronLeft, ChevronRight, Eye, RotateCw, Search, SlidersHorizontal } from "lucide-react";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { formatDuration, formatEpochMillis } from "@/shared/formatters";
import type { EventConnectionState } from "@/shared/api/events";
import type { QueryFilter, QueryRecord, QueryRequest } from "./api";
import { useQueries } from "./hooks";

const DAY_MS = 86_400_000;
const transports = ["udp", "tcp", "doh"] as const;
const sources = ["cache", "hosts", "rule", "upstream", "synthetic"] as const;
const outcomes = ["answered", "negative", "timeout", "rejected", "failed"] as const;
const caches = ["hit", "stale", "miss", "bypass"] as const;
const rcodes = ["NOERROR", "FORMERR", "SERVFAIL", "NXDOMAIN", "NOTIMP", "REFUSED", "OTHER"];

type DatePreset = "24h" | "7d" | "custom";
type Navigation = Pick<QueryRequest, "cursor" | "direction">;

interface DetailSnapshot {
  record: QueryRecord;
  directoryRevision: string;
}

export function QueriesPage() {
  const [draft, setDraft] = useState<QueryFilter>(() => defaultFilter());
  const [filter, setFilter] = useState<QueryFilter>(draft);
  const [datePreset, setDatePreset] = useState<DatePreset>("7d");
  const [advanced, setAdvanced] = useState(false);
  const [pageSize, setPageSize] = useState(20);
  const [sort, setSort] = useState<QueryRequest["sort"]>("occurred_at");
  const [order, setOrder] = useState<QueryRequest["order"]>("desc");
  const [navigation, setNavigation] = useState<Navigation>({ cursor: null, direction: "older" });
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [detail, setDetail] = useState<DetailSnapshot>();
  const [detailPinned, setDetailPinned] = useState(false);
  const detailTriggers = useRef(new Map<string, HTMLButtonElement>());
  const suppressedDetailOpen = useRef<string | undefined>(undefined);
  const request = useMemo<QueryRequest>(() => ({
    filter,
    cursor: navigation.cursor,
    direction: navigation.direction,
    page_size: pageSize,
    sort,
    order,
  }), [filter, navigation, order, pageSize, sort]);
  const latestPage = navigation.cursor === null && sort === "occurred_at" && order === "desc" && datePreset !== "custom";
  const query = useQueries(request, { autoRefresh, holdUpdates: detail !== undefined, applyImmediately: latestPage });
  const page = query.data;

  useEffect(() => {
    if (!detail) return;
    const close = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      const trigger = detailTriggers.current.get(detail.record.id);
      suppressedDetailOpen.current = detail.record.id;
      setDetail(undefined);
      setDetailPinned(false);
      window.setTimeout(() => trigger?.focus(), 0);
    };
    document.addEventListener("keydown", close);
    return () => document.removeEventListener("keydown", close);
  }, [detail]);

  const resetContext = () => {
    if (detail) suppressedDetailOpen.current = detail.record.id;
    setDetail(undefined);
    setDetailPinned(false);
    setNavigation({ cursor: null, direction: "older" });
  };

  const applyFilters = () => {
    setFilter(normalizeFilter(draft));
    resetContext();
  };

  const resetFilters = () => {
    const next = defaultFilter();
    setDraft(next);
    setFilter(next);
    setDatePreset("7d");
    setAdvanced(false);
    setSort("occurred_at");
    setOrder("desc");
    resetContext();
  };

  const choosePreset = (preset: DatePreset) => {
    setDatePreset(preset);
    if (preset === "custom") return;
    const to = utcDayEnd(Date.now());
    setDraft((current) => ({ ...current, from_ms: to - (preset === "24h" ? DAY_MS : 7 * DAY_MS), to_ms: to }));
  };

  const openDetail = (record: QueryRecord, open: boolean) => {
    if (!open) {
      if (!detailPinned && detail?.record.id === record.id) setDetail(undefined);
      return;
    }
    if (suppressedDetailOpen.current === record.id) return;
    if (detailPinned && detail?.record.id !== record.id) return;
    setDetail({
      record,
      directoryRevision: query.directoryRevisions.get(record.id) ?? page?.directory_revision ?? "unknown",
    });
  };

  const pinDetail = (record: QueryRecord) => {
    if (detailPinned && detail?.record.id === record.id) {
      suppressedDetailOpen.current = record.id;
      setDetail(undefined);
      setDetailPinned(false);
      return;
    }
    if (suppressedDetailOpen.current === record.id) suppressedDetailOpen.current = undefined;
    setDetailPinned(true);
    openDetail(record, true);
  };

  const columns = useMemo<TableColumnsType<QueryRecord>>(() => [
    {
      title: "时间",
      dataIndex: "occurred_at_ms",
      width: 168,
      responsive: ["sm"],
      render: (value: number) => <TimeCell value={value} />,
    },
    {
      title: "请求",
      key: "request",
      width: 270,
      render: (_, record) => (
        <CellStack
          primary={record.qname}
          secondary={<Space size={6} wrap><span>{record.qtype}</span><Tag color="cyan">{record.transport.toUpperCase()}</Tag></Space>}
          mono
        />
      ),
    },
    {
      title: "身份",
      key: "identity",
      width: 260,
      render: (_, record) => <IdentityCell record={record} />,
    },
    {
      title: "结果",
      key: "result",
      width: 340,
      render: (_, record) => (
        <Popover
          key={`${record.id}:${detail?.record.id === record.id ? "open" : "closed"}`}
          placement="bottom"
          trigger="hover"
          open={detail?.record.id === record.id}
          onOpenChange={(open) => openDetail(record, open)}
          destroyOnHidden
          content={detail?.record.id === record.id ? <QueryDetails snapshot={detail} /> : null}
        >
          <button
            ref={(node) => {
              if (node) detailTriggers.current.set(record.id, node);
              else detailTriggers.current.delete(record.id);
            }}
            type="button"
            className="query-result-trigger"
            aria-label={`查看 ${record.qname} 的详情`}
            onPointerEnter={() => {
              if (suppressedDetailOpen.current === record.id) suppressedDetailOpen.current = undefined;
            }}
            onClick={() => pinDetail(record)}
          >
            <ResponseCell record={record} />
            <Eye size={16} aria-hidden />
          </button>
        </Popover>
      ),
    },
    {
      title: "路由",
      key: "route",
      width: 250,
      responsive: ["lg"],
      render: (_, record) => <CellStack primary={record.strategy_name ?? "无策略"} secondary={formatRoute(record)} mono />,
    },
  ], [detail, page?.directory_revision, query.directoryRevisions]);

  const showLatest = () => {
    if (detail) suppressedDetailOpen.current = detail.record.id;
    setDetail(undefined);
    setDetailPinned(false);
    if (!latestPage) setNavigation({ cursor: null, direction: "older" });
    else void query.resynchronize();
  };

  return (
    <PageFrame
      title="解析记录"
      description="按提交后的解析事实查询请求、身份匹配、响应来源与实际路由。"
      meta={page ? (
        <Space size={12} wrap className="query-page-meta">
          <Typography.Text type="secondary">目录版本 {page.directory_revision}</Typography.Text>
          <Typography.Text type="secondary">可用起点 {formatEpochMillis(page.available_from_ms)}</Typography.Text>
        </Space>
      ) : undefined}
      actions={(
        <Space className="query-live-control" size={10} wrap>
          <Switch aria-label="自动刷新" checked={autoRefresh} onChange={setAutoRefresh} />
          <Typography.Text>自动刷新</Typography.Text>
          <ConnectionState enabled={autoRefresh} state={query.connectionState} lastUpdatedAt={query.lastUpdatedAt} />
        </Space>
      )}
    >
      <section className="query-filter-panel" aria-label="解析记录筛选">
        <div className="query-filter-primary">
          <FilterInput label="域名" value={draft.qname} placeholder="example.com" onChange={(qname) => setDraft((value) => ({ ...value, qname }))} />
          <FilterInput label="匹配客户端" value={draft.client_name} placeholder="客户端名称" onChange={(client_name) => setDraft((value) => ({ ...value, client_name }))} />
          <FilterInput label="原始 ID" value={draft.client_id} placeholder="client_id" onChange={(client_id) => setDraft((value) => ({ ...value, client_id }))} />
          <FilterInput label="原始 IP" value={draft.client_ip} placeholder="192.0.2.10" onChange={(client_ip) => setDraft((value) => ({ ...value, client_ip }))} />
          <FilterSelect label="协议" value={draft.transport} values={transports} onChange={(transport) => setDraft((value) => ({ ...value, transport }))} />
          <FilterSelect label="来源" value={draft.source} values={sources} onChange={(source) => setDraft((value) => ({ ...value, source }))} />
        </div>
        <div className="query-date-row">
          <Segmented<DatePreset>
            aria-label="日期范围"
            value={datePreset}
            options={[{ label: "24 小时", value: "24h" }, { label: "7 天", value: "7d" }, { label: "自定义", value: "custom" }]}
            onChange={choosePreset}
          />
          {datePreset === "custom" ? (
            <Space wrap>
              <label className="query-date-field">开始<input type="date" value={dateInput(draft.from_ms)} onChange={(event) => setDraft((value) => ({ ...value, from_ms: dateStart(event.target.value) }))} /></label>
              <label className="query-date-field">结束<input type="date" value={dateInput(draft.to_ms - 1)} onChange={(event) => setDraft((value) => ({ ...value, to_ms: dateStart(event.target.value) + DAY_MS }))} /></label>
            </Space>
          ) : null}
          <Button type="text" icon={<SlidersHorizontal size={16} />} onClick={() => setAdvanced((value) => !value)}>高级筛选</Button>
          <Space className="query-filter-actions">
            <Button onClick={resetFilters}>重置</Button>
            <Button type="primary" icon={<Search size={16} />} onClick={applyFilters}>查询</Button>
          </Space>
        </div>
        {advanced ? (
          <div className="query-filter-advanced">
            <FilterInput label="历史匹配 ID" value={draft.matched_client_id} placeholder="matched_client_id" onChange={(matched_client_id) => setDraft((value) => ({ ...value, matched_client_id }))} />
            <FilterInput label="QTYPE" value={draft.qtype} placeholder="A" onChange={(qtype) => setDraft((value) => ({ ...value, qtype }))} />
            <FilterSelect label="RCODE" value={draft.rcode} values={rcodes} onChange={(rcode) => setDraft((value) => ({ ...value, rcode }))} />
            <FilterSelect label="结果状态" value={draft.outcome} values={outcomes} onChange={(outcome) => setDraft((value) => ({ ...value, outcome }))} />
            <FilterSelect label="缓存状态" value={draft.cache} values={caches} onChange={(cache) => setDraft((value) => ({ ...value, cache }))} />
            <FilterSelect label="排序字段" value={sort} values={["occurred_at", "duration"] as const} onChange={(value) => { setSort(value ?? "occurred_at"); resetContext(); }} />
            <FilterSelect label="排序方向" value={order} values={["desc", "asc"] as const} onChange={(value) => { setOrder(value ?? "desc"); resetContext(); }} />
          </div>
        ) : null}
      </section>

      {autoRefresh && (query.requiresResync || (query.pendingCount ?? 0) > 0) ? (
        <Alert
          className="query-update-alert"
          type={query.requiresResync ? "warning" : "info"}
          showIcon
          title={query.requiresResync ? "有新记录，需重新同步" : `有 ${query.pendingCount} 条新记录`}
          action={<Button size="small" icon={<RotateCw size={15} />} onClick={showLatest}>查看新记录</Button>}
        />
      ) : null}

      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {page ? (
        <section className="query-records" aria-label="解析记录列表">
          <PageState empty={query.items.length === 0} emptyDescription="当前筛选条件下没有解析记录" />
          {query.items.length > 0 ? (
            <Table
              rowKey="id"
              columns={columns}
              dataSource={query.items}
              scroll={{ x: 1_150 }}
              pagination={false}
              loading={query.isFetching && !query.isLoading}
            />
          ) : null}
          <div className="query-pagination">
            <Typography.Text type="secondary">当前显示 {query.items.length} 条</Typography.Text>
            <Space>
              <Select
                aria-label="每页条数"
                value={pageSize}
                options={[10, 20, 50, 100].map((value) => ({ value, label: `${value} 条/页` }))}
                onChange={(value) => { setPageSize(value); resetContext(); }}
              />
              <Button
                aria-label="上一页"
                icon={<ChevronLeft size={16} />}
                disabled={!page.previous_cursor || query.isFetching}
                onClick={() => { if (detail) suppressedDetailOpen.current = detail.record.id; setDetail(undefined); setDetailPinned(false); setNavigation({ cursor: page.previous_cursor, direction: "newer" }); }}
              />
              <Button
                aria-label="下一页"
                icon={<ChevronRight size={16} />}
                disabled={!page.next_cursor || query.isFetching}
                onClick={() => { if (detail) suppressedDetailOpen.current = detail.record.id; setDetail(undefined); setDetailPinned(false); setNavigation({ cursor: page.next_cursor, direction: "older" }); }}
              />
            </Space>
          </div>
        </section>
      ) : null}
    </PageFrame>
  );
}

function TimeCell({ value }: { value: number }) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return <Typography.Text type="secondary">—</Typography.Text>;
  return <CellStack primary={date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })} secondary={date.toLocaleDateString()} />;
}

function IdentityCell({ record }: { record: QueryRecord }) {
  const matched = record.matched.source === "none"
    ? "当时未匹配"
    : `当时按 ${record.matched.source.toUpperCase()} 匹配 ${record.matched.matched_client_id}`;
  return (
    <CellStack
      primary={record.identity.client_id ?? "未传入原始 ID"}
      secondary={<><span>{record.identity.client_ip}</span><span>{matched}{record.current_client_name ? ` · 当前 ${record.current_client_name}` : ""}</span></>}
      mono
    />
  );
}

function ResponseCell({ record }: { record: QueryRecord }) {
  const summary = formatResponseSummary(record);
  const durations = formatDurationSummary(record);
  return (
    <CellStack
      primary={summary.primary}
      secondary={<Space size={6} wrap><span>{durations.total}</span><span>{durations.dnsCore}</span><span>{summary.meta}</span><Tag color={record.source === "cache" ? "green" : "blue"}>{sourceLabel(record)}</Tag></Space>}
      mono={record.answers.state !== "unavailable" && record.answers.records.length > 0}
    />
  );
}

function QueryDetails({ snapshot }: { snapshot: DetailSnapshot }) {
  const { record } = snapshot;
  const answers = record.answers.state === "unavailable" ? [] : record.answers.records;
  return (
    <div className="query-details-popover" role="dialog" aria-label={`${record.qname} 解析详情`}>
      <div className="query-detail-heading">
        <Typography.Title level={5}>{record.qname}</Typography.Title>
        <Typography.Text type="secondary" code>{record.id}</Typography.Text>
      </div>
      <Descriptions size="small" column={1} colon={false}>
        <Descriptions.Item label="发生时间">{formatEpochMillis(record.occurred_at_ms)}</Descriptions.Item>
        <Descriptions.Item label="请求">{record.qtype} / {record.transport.toUpperCase()}</Descriptions.Item>
        <Descriptions.Item label="原始身份">{record.identity.client_id ?? "未传入 ID"} · {record.identity.client_ip}</Descriptions.Item>
        <Descriptions.Item label="当时匹配">{formatHistoricalMatch(record)}</Descriptions.Item>
        <Descriptions.Item label="当前名称">{record.current_client_name ?? "配置中已无对应名称"}</Descriptions.Item>
        <Descriptions.Item label="响应">{record.rcode} / {record.outcome} / {record.cache}</Descriptions.Item>
        <Descriptions.Item label="耗时">{formatDurationSummary(record).total} · {formatDurationSummary(record).dnsCore}</Descriptions.Item>
        <Descriptions.Item label="路由">{record.strategy_name ?? "无策略"} · {formatRoute(record)}</Descriptions.Item>
        <Descriptions.Item label="目录快照">{snapshot.directoryRevision}</Descriptions.Item>
      </Descriptions>
      {record.source === "cache" ? <Alert type="info" showIcon title="上游信息来自缓存生产请求，本次解析未再次访问上游" /> : null}
      <div className="query-answer-list">
        <Typography.Text strong>Answer</Typography.Text>
        {record.answers.state === "unavailable" ? <Typography.Text type="secondary">结果未保留</Typography.Text> : null}
        {record.answers.state === "truncated" ? <Alert type="warning" showIcon title={`保留 ${answers.length} 条，共 ${record.answers.total_count} 条`} /> : null}
        {record.answers.state !== "unavailable" && answers.length === 0 ? <Typography.Text type="secondary">响应没有 Answer 记录</Typography.Text> : null}
        {answers.map((answer, index) => (
          <div className="query-answer-row" key={`${answer.name}:${answer.type}:${index}`}>
            <span>{answer.type}</span><code>{answer.name}</code><code>{answer.data}</code><span>{answer.ttl_seconds}s</span>
          </div>
        ))}
      </div>
    </div>
  );
}

export function formatDurationSummary(record: QueryRecord): { total: string; dnsCore: string } {
  return {
    total: `总耗时 ${formatDuration(record.duration_us === null ? null : record.duration_us / 1_000)}`,
    dnsCore: `主链 ${formatDuration(record.dns_core_duration_us === null ? null : record.dns_core_duration_us / 1_000)}`,
  };
}

export function formatRoute(record: QueryRecord): string {
  if (record.source === "hosts" || record.source === "rule" || record.source === "synthetic") return "本地响应";
  if (record.source === "cache") {
    const producer = record.cache_producer;
    if (!producer?.upstream_target_name) return "缓存生产上游未确定";
    return `缓存生产：${formatUpstream(producer.upstream_target_name, producer.upstream_used_name)}`;
  }
  return record.upstream_target_name
    ? formatUpstream(record.upstream_target_name, record.upstream_used_name)
    : "upstream 未确定";
}

export function formatClient(record: QueryRecord): { primary: string; secondary: string; muted: boolean } {
  return {
    primary: record.identity.client_id ?? "未传入原始 ID",
    secondary: record.identity.client_ip,
    muted: record.identity.client_id === null,
  };
}

export function formatResponseSummary(record: QueryRecord): { primary: string; meta: string } {
  const answer = record.answers.state === "unavailable" ? undefined : record.answers.records[0];
  return {
    primary: answer ? `${answer.type}  ${answer.data}` : `${record.rcode} · ${record.outcome}`,
    meta: record.answers.state === "unavailable"
      ? "结果未保留"
      : record.answers.state === "truncated"
        ? `保留 ${record.answers.records.length} 条，共 ${record.answers.total_count} 条`
        : `${record.answers.total_count} 条结果`,
  };
}

function formatUpstream(target: string, used: string | null): string {
  return used && used !== target ? `${target} → ${used}` : used ?? `${target} → 未确定`;
}

function formatHistoricalMatch(record: QueryRecord): string {
  return record.matched.source === "none"
    ? "未匹配"
    : `${record.matched.matched_client_id}（${record.matched.source.toUpperCase()}）`;
}

function sourceLabel(record: QueryRecord): string {
  if (record.source === "cache") return record.cache === "stale" ? "过期缓存" : "缓存命中";
  return record.source === "rule" ? "规则" : record.source;
}

function CellStack({ primary, secondary, muted = false, mono = false }: { primary: ReactNode; secondary?: ReactNode; muted?: boolean; mono?: boolean }) {
  return (
    <div className={`query-cell${mono ? " query-mono" : ""}`}>
      <Typography.Text type={muted ? "secondary" : undefined}>{primary}</Typography.Text>
      {secondary ? <Typography.Text type="secondary" className="query-cell-secondary">{secondary}</Typography.Text> : null}
    </div>
  );
}

function FilterInput({ label, value, placeholder, onChange }: { label: string; value?: string; placeholder: string; onChange: (value?: string) => void }) {
  return (
    <div className="filter-field">
      <label>{label}</label>
      <Input allowClear aria-label={label} value={value ?? ""} placeholder={placeholder} onChange={(event) => onChange(event.target.value || undefined)} />
    </div>
  );
}

function FilterSelect<T extends string>({ label, value, values, onChange }: { label: string; value: T | undefined; values: readonly T[]; onChange: (value: T | undefined) => void }) {
  return (
    <div className="filter-field">
      <label>{label}</label>
      <Select allowClear aria-label={label} placeholder="全部" value={value} options={values.map((item) => ({ value: item, label: item.toUpperCase() }))} onChange={onChange} />
    </div>
  );
}

function ConnectionState({ enabled, state, lastUpdatedAt }: { enabled: boolean; state: EventConnectionState; lastUpdatedAt?: number }) {
  if (!enabled) return <Typography.Text type="secondary">已关闭</Typography.Text>;
  const label = state === "open" ? "实时连接正常" : state === "connecting" ? "连接中" : state === "reconnecting" ? "正在重连" : "实时连接异常";
  return (
    <Typography.Text type={state === "error" ? "danger" : "secondary"} className={`query-connection query-connection-${state}`} role="status">
      {label}{lastUpdatedAt ? ` · ${new Date(lastUpdatedAt).toLocaleTimeString()}` : ""}
    </Typography.Text>
  );
}

function defaultFilter(now = Date.now()): QueryFilter {
  const to = utcDayEnd(now);
  return { from_ms: to - 7 * DAY_MS, to_ms: to };
}

function utcDayEnd(value: number): number {
  const date = new Date(value);
  return Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate() + 1);
}

function normalizeFilter(filter: QueryFilter): QueryFilter {
  const normalized = { ...filter };
  for (const key of ["client_id", "client_ip", "qname", "matched_client_id", "client_name", "qtype", "rcode"] as const) {
    const value = normalized[key]?.trim();
    if (value) normalized[key] = value;
    else delete normalized[key];
  }
  return normalized;
}

function dateInput(value: number): string {
  return new Date(value).toISOString().slice(0, 10);
}

function dateStart(value: string): number {
  const parsed = Date.parse(`${value}T00:00:00Z`);
  return Number.isFinite(parsed) ? parsed : 0;
}
