import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
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
import { ChevronLeft, ChevronRight, Clock3, Eye, RotateCw, Search, SlidersHorizontal } from "lucide-react";
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
const caches = ["hit", "stale", "expired", "miss", "bypass"] as const;
/** 缓存状态筛选显示名与结果列来源标签一致；筛选值仍提交枚举原文。 */
const cacheLabels: Record<(typeof caches)[number], string> = {
  hit: "命中缓存",
  stale: "乐观缓存",
  expired: "缓存过期",
  miss: "未命中",
  bypass: "未启用",
};
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
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [detail, setDetail] = useState<DetailSnapshot>();
  const [detailPinned, setDetailPinned] = useState(false);
  const detailTriggers = useRef(new Map<string, HTMLButtonElement>());
  const suppressedDetailOpen = useRef<string | undefined>(undefined);
  const detailContent = useRef<HTMLDivElement>(null);
  const hoverCloseTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const hoverOpenTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const pinnedRef = useRef(detailPinned);
  pinnedRef.current = detailPinned;
  const cancelHoverClose = () => { clearTimeout(hoverCloseTimer.current); };
  const scheduleHoverClose = (record: QueryRecord) => {
    cancelHoverClose();
    hoverCloseTimer.current = setTimeout(() => {
      if (!pinnedRef.current) setDetail((current) => current?.record.id === record.id ? undefined : current);
    }, 180);
  };
  useEffect(() => () => {
    clearTimeout(hoverCloseTimer.current);
    clearTimeout(hoverOpenTimer.current);
  }, []);
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
      const recordId = detail.record.id;
      suppressedDetailOpen.current = detail.record.id;
      setDetail(undefined);
      setDetailPinned(false);
      // 关闭后按稳定 ID 重新取得触发节点，避免列表合并时恢复到失效 DOM。
      window.setTimeout(() => detailTriggers.current.get(recordId)?.focus(), 0);
    };
    document.addEventListener("keydown", close);
    const outside = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node) || detailContent.current?.contains(target)
        || (target instanceof Element && target.closest(".query-result-trigger"))) return;
      suppressedDetailOpen.current = detail.record.id;
      setDetail(undefined);
      setDetailPinned(false);
    };
    document.addEventListener("pointerdown", outside, true);
    return () => {
      document.removeEventListener("keydown", close);
      document.removeEventListener("pointerdown", outside, true);
    };
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
      if (!detailPinned) setDetail((current) => current?.record.id === record.id ? undefined : current);
      return;
    }
    if (suppressedDetailOpen.current === record.id) return;
    if (detailPinned) return;
    setDetail((current) => current?.record.id === record.id ? current : {
      record,
      directoryRevision: query.directoryRevisions.get(record.id) ?? page?.directory_revision ?? "unknown",
    });
  };

  const pinDetail = (record: QueryRecord) => {
    clearTimeout(hoverOpenTimer.current);
    cancelHoverClose();
    suppressedDetailOpen.current = undefined;
    setDetailPinned(true);
    // 点击必须能够切换已固定记录，不能走仅处理 hover 的 pinned 防护。
    setDetail((current) => current?.record.id === record.id ? current : {
      record,
      directoryRevision: query.directoryRevisions.get(record.id) ?? page?.directory_revision ?? "unknown",
    });
  };

  const columns: TableColumnsType<QueryRecord> = [
    {
      title: "时间",
      dataIndex: "occurred_at_ms",
      width: 145,
      render: (value: number) => <TimeCell value={value} />,
    },
    {
      title: "请求",
      key: "request",
      width: 260,
      render: (_, record) => (
        <CellStack
          primary={record.qname}
          secondary={<><span>{record.qtype}</span><Tag color="cyan">{record.transport.toUpperCase()}</Tag></>}
        />
      ),
    },
    {
      title: "结果",
      key: "result",
      className: "query-result-cell",
      width: 340,
      render: (_, record) => (
        <Popover
          key={record.id}
          placement="bottom"
          trigger={[]}
          fresh
          open={detail?.record.id === record.id}
          destroyOnHidden
          content={detail?.record.id === record.id ? <div ref={detailContent} onPointerEnter={cancelHoverClose} onPointerLeave={() => scheduleHoverClose(record)}><QueryDetails snapshot={detail} pinned={detailPinned} /></div> : null}
        >
          <button
            ref={(node) => {
              if (node) detailTriggers.current.set(record.id, node);
              else detailTriggers.current.delete(record.id);
            }}
            type="button"
            className="query-result-trigger"
            aria-label={`查看 ${record.qname} 的详情`}
            aria-expanded={detail?.record.id === record.id}
            aria-haspopup="dialog"
            onPointerEnter={(event) => {
              if (event.pointerType !== "mouse") return;
              if (suppressedDetailOpen.current === record.id) suppressedDetailOpen.current = undefined;
              cancelHoverClose();
              clearTimeout(hoverOpenTimer.current);
              hoverOpenTimer.current = setTimeout(() => {
                if (!pinnedRef.current) openDetail(record, true);
              }, 120);
            }}
            onPointerLeave={() => { clearTimeout(hoverOpenTimer.current); scheduleHoverClose(record); }}
            onPointerDown={() => clearTimeout(hoverOpenTimer.current)}
            onClick={() => pinDetail(record)}
            onFocus={(event) => {
              // 鼠标/触摸按下的 focus 不能提前弹出并挡住随后的 pointerup/click。
              if (event.currentTarget.matches(":focus-visible")) openDetail(record, true);
            }}
            onBlur={() => openDetail(record, false)}
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
      width: 300,
      render: (_, record) => <CellStack primary={<RouteChain record={record} />} secondary={<CacheActivityTags record={record} />} />,
    },
    {
      title: "客户端",
      key: "identity",
      width: 200,
      render: (_, record) => <IdentityCell record={record} />,
    },
  ];

  const showLatest = () => {
    if (detail) suppressedDetailOpen.current = detail.record.id;
    setDetail(undefined);
    setDetailPinned(false);
    if (!latestPage) setNavigation({ cursor: null, direction: "older" });
    else void query.resynchronize();
  };

  return (
    <div className="query-page">
    <PageFrame
      title="解析记录"
      description="每一次请求，清晰可循。"
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
          <FilterInput label="客户端" value={draft.client_name} placeholder="客户端名称" onChange={(client_name) => setDraft((value) => ({ ...value, client_name }))} />
          <FilterInput label="请求 IP" value={draft.client_ip} placeholder="192.0.2.10" onChange={(client_ip) => setDraft((value) => ({ ...value, client_ip }))} />
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
            <FilterInput label="原始 ID" value={draft.client_id} placeholder="client_id" onChange={(client_id) => setDraft((value) => ({ ...value, client_id }))} />
            <FilterInput label="历史匹配 ID" value={draft.matched_client_id} placeholder="matched_client_id" onChange={(matched_client_id) => setDraft((value) => ({ ...value, matched_client_id }))} />
            <FilterInput label="QTYPE" value={draft.qtype} placeholder="A" onChange={(qtype) => setDraft((value) => ({ ...value, qtype }))} />
            <FilterSelect label="RCODE" value={draft.rcode} values={rcodes} onChange={(rcode) => setDraft((value) => ({ ...value, rcode }))} />
            <FilterSelect label="结果状态" value={draft.outcome} values={outcomes} onChange={(outcome) => setDraft((value) => ({ ...value, outcome }))} />
            <FilterSelect label="缓存状态" value={draft.cache} values={caches} labels={cacheLabels} onChange={(cache) => setDraft((value) => ({ ...value, cache }))} />
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
              tableLayout="fixed"
              rowClassName={(record) => record.id === detail?.record.id ? "query-row-active" : ""}
              scroll={{ x: 1_245 }}
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
    </div>
  );
}

function TimeCell({ value }: { value: number }) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return <Typography.Text type="secondary">—</Typography.Text>;
  return <CellStack primary={date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })} secondary={date.toLocaleDateString()} />;
}

function IdentityCell({ record }: { record: QueryRecord }) {
  const identity = formatClientIdentity(record);
  return (
    <CellStack
      primary={identity.primary}
      secondary={identity.clientIp}
    />
  );
}

function ResponseCell({ record }: { record: QueryRecord }) {
  const summary = formatResponseSummary(record);
  const durations = formatDurationSummary(record);
  const source = sourceLabel(record);
  return (
    <CellStack
      primary={summary.primary}
      secondary={<><Clock3 size={13} aria-hidden /><span>{durations.response}</span><Tag color={source.color}>{source.label}</Tag></>}
    />
  );
}

function QueryDetails({ snapshot, pinned }: { snapshot: DetailSnapshot; pinned: boolean }) {
  const { record } = snapshot;
  const answers = record.answers.state === "unavailable" ? [] : record.answers.records;
  const durations = formatDurationSummary(record);
  return (
    <div className="query-details-popover" role="dialog" aria-label={`${record.qname} 解析详情`}>
      <div className="query-detail-heading">
        <Typography.Title level={5}>{record.qname}</Typography.Title>
        <Tag color={pinned ? "blue" : undefined}>{pinned ? "点击固定" : "悬停预览"}</Tag>
      </div>
      <Typography.Text type="secondary" className="query-detail-id" code>{record.id}</Typography.Text>
      <div className="query-detail-durations">
        {[durations.total, durations.dnsCore, durations.response].map((duration) => {
          const [label, ...value] = duration.split(" ");
          const hint = label === "响应耗时" ? "收到完整请求至服务端成功写出响应，不含后台刷新，也不是客户端接收确认" : label === "总耗时" ? "Transport 接入计时点至 DNS core 完成" : "仅 DNS core 主链解析耗时";
          return <div key={label} title={hint}><span>{label}</span><strong>{value.join(" ")}</strong></div>;
        })}
      </div>
      <Descriptions size="small" column={1} colon={false}>
        <Descriptions.Item label="发生时间">{formatEpochMillis(record.occurred_at_ms)}</Descriptions.Item>
        <Descriptions.Item label="请求">{record.qtype} / {record.transport.toUpperCase()}</Descriptions.Item>
        <Descriptions.Item label="原始身份">{record.identity.client_id ?? "未传入 ID"} · {record.identity.client_ip}</Descriptions.Item>
        <Descriptions.Item label="当时匹配">{formatHistoricalMatch(record)}</Descriptions.Item>
        <Descriptions.Item label="当前名称">{record.current_client_name ?? "配置中已无对应名称"}</Descriptions.Item>
        <Descriptions.Item label="响应">{record.rcode} / {record.outcome} / {sourceLabel(record).label}</Descriptions.Item>
        <Descriptions.Item label="发送状态">{responseStatusLabel(record)}</Descriptions.Item>
        <Descriptions.Item label="路由链路"><span className="query-detail-route">{formatRoute(record)}</span></Descriptions.Item>
        <Descriptions.Item label="缓存变化"><CacheActivityTags record={record} />{!record.cache_activity ? "无记录" : null}</Descriptions.Item>
        {record.cache_activity?.upstream_target_name ? <Descriptions.Item label="刷新链路">{[record.listener_name, record.cache_activity.upstream_target_name, record.cache_activity.upstream_used_name].filter(Boolean).join(" → ")}</Descriptions.Item> : null}
        <Descriptions.Item label="目录快照">{snapshot.directoryRevision}</Descriptions.Item>
      </Descriptions>
      {record.source === "cache" ? <Alert type="info" showIcon title={record.cache === "stale"
        ? "本次先返回过期缓存，后台尝试刷新；响应耗时不包含后台刷新。链路出口来自旧缓存生产请求，刷新链路单独展示。"
        : "链路出口来自缓存生产请求，本次直接命中缓存。"} /> : null}
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

export function formatDurationSummary(record: QueryRecord): { total: string; dnsCore: string; response: string } {
  return {
    total: `总耗时 ${formatDuration(record.duration_us === null ? null : record.duration_us / 1_000)}`,
    dnsCore: `主链耗时 ${formatDuration(record.dns_core_duration_us === null ? null : record.dns_core_duration_us / 1_000)}`,
    response: `响应耗时 ${record.response_duration_us == null ? "未记录" : formatDuration(record.response_duration_us / 1_000)}`,
  };
}

export function formatRoute(record: QueryRecord): string {
  return routeNodes(record).join(" → ");
}

/** 链路保留真实入口和出口；缓存来源不改写成虚构的“缓存”节点。 */
function routeNodes(record: QueryRecord): string[] {
  const producer = record.source === "cache" ? record.cache_producer : record;
  const nodes: (string | null | undefined)[] = [record.listener_name ?? "入口未记录", record.strategy_name];
  if (["hosts", "rule", "synthetic"].includes(record.source)) nodes.push(record.source === "hosts" ? "Hosts" : "本地响应");
  else nodes.push(producer?.upstream_target_name, producer?.upstream_used_name ?? "上游未确定");
  return nodes.filter((value): value is string => !!value).filter((value, index, values) => index === 0 || value !== values[index - 1]);
}

/** 根据实际列宽决定中间省略；首尾各自可截断，但不会把出口挤出列。 */
function RouteChain({ record }: { record: QueryRecord }) {
  const host = useRef<HTMLSpanElement>(null);
  const measure = useRef<HTMLSpanElement>(null);
  const [compact, setCompact] = useState(false);
  const nodes = routeNodes(record);
  const full = nodes.join(" → ");
  useLayoutEffect(() => {
    const update = () => setCompact((measure.current?.scrollWidth ?? 0) > (host.current?.clientWidth ?? 0));
    update();
    const observer = new ResizeObserver(update);
    if (host.current) observer.observe(host.current);
    return () => observer.disconnect();
  }, [full]);
  return <span className="query-route-chain" ref={host} title={full} aria-label={full}>
    <span className="query-route-measure" ref={measure} aria-hidden>{full}</span>
    {compact ? <><span className="query-route-endpoint">{nodes[0]}</span><span className="query-route-gap">{nodes.length > 2 ? "→ … →" : "→"}</span><span className="query-route-endpoint">{nodes.at(-1)}</span></> : <span className="query-route-full">{full}</span>}
  </span>;
}

/** 仅使用后端写入结果展示变更；Hosts、本地响应或历史无记录不增加无意义标签。 */
function CacheActivityTags({ record }: { record: QueryRecord }) {
  const activity = record.cache_activity;
  if (!activity || ["hosts", "rule", "synthetic"].includes(record.source)) return null;
  const labels: Record<typeof activity.outcome, string> = {
    pending: "等待写入", inserted: "新建缓存", updated: "更新缓存", rejected: "写入拒绝",
    conflict: "写入冲突", failed: "写入失败", skipped: "跳过刷新", coalesced: "合并刷新",
    dropped: "任务丢弃", unrecorded: "结果未知",
  };
  return <>{activity.kind === "refresh" ? <Tag color="blue">后台刷新</Tag> : null}
    <Tag color={activity.outcome === "inserted" ? "green" : activity.outcome === "updated" ? "blue" : ["failed", "dropped"].includes(activity.outcome) ? "orange" : "default"}>{labels[activity.outcome]}</Tag></>;
}

function responseStatusLabel(record: QueryRecord): string {
  return ({ sent: "已写出", failed: "发送失败", cancelled: "已取消", pending: "发送中", unrecorded: "未记录" })[record.response_status];
}

/** 列表仅展示名称与 IP，历史匹配 ID 保留在详情中。 */
export function formatClientIdentity(record: QueryRecord): {
  primary: string;
  clientIp: string;
  detail: string;
} {
  const matched = record.matched;
  const primary = record.current_client_name ?? (matched.source === "none" ? "未匹配客户端" : "未命名客户端");
  const detail = matched.source === "none"
    ? "当时未匹配"
    : `当时按 ${matched.source.toUpperCase()} 匹配 ${matched.matched_client_id}`;
  return {
    primary,
    clientIp: record.identity.client_ip,
    detail,
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

function formatHistoricalMatch(record: QueryRecord): string {
  return record.matched.source === "none"
    ? "未匹配"
    : `${record.matched.matched_client_id}（${record.matched.source.toUpperCase()}）`;
}

export function sourceLabel(record: QueryRecord): { label: string; color: string } {
  if (record.cache === "hit") return { label: "命中缓存", color: "green" };
  if (record.cache === "stale") return { label: "乐观缓存", color: "gold" };
  if (record.cache === "expired") return { label: "缓存过期", color: "orange" };
  if (record.source === "upstream") return { label: "请求上游", color: "blue" };
  if (record.source === "hosts") return { label: "Hosts", color: "purple" };
  if (record.source === "rule") return { label: "规则", color: "blue" };
  return { label: record.source, color: "blue" };
}

function CellStack({ primary, secondary, muted = false, mono = false }: { primary: ReactNode; secondary?: ReactNode; muted?: boolean; mono?: boolean }) {
  return (
    <div className={`query-cell${mono ? " query-mono" : ""}`}>
      <span className={`query-cell-primary${muted ? " query-cell-muted" : ""}`} title={typeof primary === "string" ? primary : undefined}>{primary}</span>
      <span className="query-cell-secondary">{secondary}</span>
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

function FilterSelect<T extends string>({ label, value, values, labels, onChange }: { label: string; value: T | undefined; values: readonly T[]; labels?: Partial<Record<T, string>>; onChange: (value: T | undefined) => void }) {
  return (
    <div className="filter-field">
      <label>{label}</label>
      <Select allowClear aria-label={label} placeholder="全部" value={value} options={values.map((item) => ({ value: item, label: labels?.[item] ?? item.toUpperCase() }))} onChange={onChange} />
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
