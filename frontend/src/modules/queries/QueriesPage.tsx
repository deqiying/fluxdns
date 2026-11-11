import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Alert,
  Button,
  Descriptions,
  Drawer,
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
import { ChevronLeft, ChevronRight, Copy, Eye, Pin, RotateCw, Search, SlidersHorizontal, X } from "lucide-react";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { formatDuration } from "@/shared/formatters";
import { useServerTimeZone } from "@/shared/time/TimeZoneContext";
import { calendarDayRange, calendarDayStart, formatTimeParts, shiftCalendarDate, timeZoneLabel } from "@/shared/time/time-zone";
import type { EventConnectionState } from "@/shared/api/events";
import type { QueryFilter, QueryRecord, QueryRequest } from "./api";
import { useQueries } from "./hooks";
import { QueryEcsDiagnostics, QueryEcsSummary } from "./QueryEcsSummary";
import "./queries.css";

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

interface QueryPreferences {
  advanced: boolean;
  pageSize: number;
  sort: QueryRequest["sort"];
  order: QueryRequest["order"];
  autoRefresh: boolean;
}

interface DetailSnapshot {
  record: QueryRecord;
  directoryRevision: string;
  items: QueryRecord[];
  directoryRevisions: ReadonlyMap<string, string>;
}

export function QueriesPage() {
  const { timeZone } = useServerTimeZone();
  const [preferences, setPreferences] = useState<QueryPreferences>({ advanced: false, pageSize: 10, sort: "occurred_at", order: "desc", autoRefresh: true });
  const updatePreferences = (changes: Partial<QueryPreferences>) => setPreferences((current) => ({ ...current, ...changes }));
  // 时区变化后重新建立日期/cursor/实时订阅上下文，不沿用旧时区解释的筛选或详情。
  return <QueriesContent key={timeZone} timeZone={timeZone} preferences={preferences} updatePreferences={updatePreferences} />;
}

function QueriesContent({ timeZone, preferences, updatePreferences }: { timeZone: string; preferences: QueryPreferences; updatePreferences: (changes: Partial<QueryPreferences>) => void }) {
  const { advanced, pageSize, sort, order, autoRefresh } = preferences;
  const drawerTitleId = useId();
  const [draft, setDraft] = useState<QueryFilter>(() => defaultFilter(timeZone));
  const [filter, setFilter] = useState<QueryFilter>(draft);
  const [datePreset, setDatePreset] = useState<DatePreset>("7d");
  const [navigation, setNavigation] = useState<Navigation>({ cursor: null, direction: "older" });
  const [detail, setDetail] = useState<DetailSnapshot>();
  const [detailPinned, setDetailPinned] = useState(false);
  const narrow = useNarrowQueries();
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
  // HTTP refetch 也可能移走触发行；详情期间冻结列表，确保浮层及焦点锚点仍在。
  const visibleItems = detail?.items ?? query.items;

  const closeDetail = (restoreFocus = true) => {
    clearTimeout(hoverOpenTimer.current);
    cancelHoverClose();
    const recordId = detail?.record.id;
    suppressedDetailOpen.current = recordId;
    setDetail(undefined);
    setDetailPinned(false);
    // 显式或键盘关闭后按 ID 恢复节点；外部指针关闭应保留用户新选中的输入焦点。
    if (restoreFocus && recordId) window.setTimeout(() => detailTriggers.current.get(recordId)?.focus(), 0);
  };

  useEffect(() => {
    if (!detail || narrow) return;
    const close = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      closeDetail();
    };
    document.addEventListener("keydown", close);
    const outside = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node) || detailContent.current?.contains(target)
        || (target instanceof Element && target.closest(".query-result-trigger"))) return;
      closeDetail(false);
    };
    document.addEventListener("pointerdown", outside, true);
    return () => {
      document.removeEventListener("keydown", close);
      document.removeEventListener("pointerdown", outside, true);
    };
  }, [detail, narrow]);

  const resetContext = () => {
    clearTimeout(hoverOpenTimer.current);
    cancelHoverClose();
    if (detail) suppressedDetailOpen.current = detail.record.id;
    setDetail(undefined);
    setDetailPinned(false);
    setNavigation({ cursor: null, direction: "older" });
  };
  const validDateRange = Number.isFinite(draft.from_ms) && Number.isFinite(draft.to_ms) && draft.from_ms < draft.to_ms;

  const applyFilters = () => {
    if (!validDateRange) return;
    setFilter(normalizeFilter(draft));
    resetContext();
  };

  const resetFilters = () => {
    const next = defaultFilter(timeZone);
    setDraft(next);
    setFilter(next);
    setDatePreset("7d");
    updatePreferences({ advanced: false, sort: "occurred_at", order: "desc" });
    resetContext();
  };

  const choosePreset = (preset: DatePreset) => {
    setDatePreset(preset);
    if (preset === "custom") return;
    setDraft((current) => ({ ...current, ...calendarDayRange(Date.now(), preset === "24h" ? 1 : 7, timeZone) }));
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
      directoryRevision: (current?.directoryRevisions ?? query.directoryRevisions).get(record.id) ?? page?.directory_revision ?? "unknown",
      items: current?.items ?? query.items,
      directoryRevisions: current?.directoryRevisions ?? new Map(query.directoryRevisions),
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
      directoryRevision: (current?.directoryRevisions ?? query.directoryRevisions).get(record.id) ?? page?.directory_revision ?? "unknown",
      items: current?.items ?? query.items,
      directoryRevisions: current?.directoryRevisions ?? new Map(query.directoryRevisions),
    });
  };

  const columns: TableColumnsType<QueryRecord> = [
    {
      title: "时间",
      dataIndex: "occurred_at_ms",
      width: 112,
      render: (value: number) => <TimeCell value={value} />,
    },
    {
      title: "请求",
      key: "request",
      width: 220,
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
      width: 260,
      render: (_, record) => (
        <Popover
          key={record.id}
          placement="rightTop"
          autoAdjustOverflow
          classNames={{ root: "query-detail-overlay" }}
          trigger={[]}
          fresh
          open={!narrow && detail?.record.id === record.id}
          destroyOnHidden
          content={detail?.record.id === record.id ? <div ref={detailContent} onPointerEnter={cancelHoverClose} onPointerLeave={() => scheduleHoverClose(record)}><QueryDetails key={detail.record.id} snapshot={detail} pinned={detailPinned} onPin={() => setDetailPinned((value) => !value)} onClose={() => closeDetail()} /></div> : null}
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
              if (narrow || event.pointerType !== "mouse") return;
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
              if (!narrow && event.currentTarget.matches(":focus-visible")) openDetail(record, true);
            }}
            onBlur={(event) => {
              if (event.relatedTarget instanceof Node && detailContent.current?.contains(event.relatedTarget)) return;
              openDetail(record, false);
            }}
          >
            <ResponseCell record={record} />
            <Eye size={16} aria-hidden />
          </button>
        </Popover>
      ),
    },
    {
      title: "响应耗时",
      key: "response-duration",
      width: 120,
      align: "right",
      render: (_, record) => <span className="query-mono" aria-label={formatDurationSummary(record).response}>{responseDuration(record)}</span>,
    },
    {
      title: "路由",
      key: "route",
      width: 250,
      render: (_, record) => <CellStack primary={<RouteChain record={record} />} secondary={<><CacheActivityTags record={record} /><RouteSourceTag record={record} /></>} />,
    },
    {
      title: "客户端",
      key: "identity",
      width: 170,
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
          <Switch aria-label="自动刷新" checked={autoRefresh} onChange={(autoRefresh) => updatePreferences({ autoRefresh })} />
          <Typography.Text>自动刷新</Typography.Text>
          <ConnectionState enabled={autoRefresh} state={query.connectionState} lastUpdatedAt={query.lastUpdatedAt} />
        </Space>
      )}
    >
      <section className="query-filter-panel" aria-label="解析记录筛选">
        <div className="query-filter-primary">
          <FilterInput label="域名" value={draft.qname} placeholder="域名关键词" onChange={(qname) => setDraft((value) => ({ ...value, qname }))} />
          <FilterInput label="客户端" value={draft.client_name} placeholder="客户端名称关键词" onChange={(client_name) => setDraft((value) => ({ ...value, client_name }))} />
          <FilterInput label="请求 IP" value={draft.client_ip} placeholder="IP 或地址片段" onChange={(client_ip) => setDraft((value) => ({ ...value, client_ip }))} />
          <FilterSelect label="协议" value={draft.transport} values={transports} onChange={(transport) => setDraft((value) => ({ ...value, transport }))} />
          <FilterSelect label="来源" value={draft.source} values={sources} onChange={(source) => setDraft((value) => ({ ...value, source }))} />
        </div>
        <div className="query-date-row">
          <Segmented<DatePreset>
            aria-label="日期范围"
            value={datePreset}
            options={[{ label: "今天", value: "24h" }, { label: "7 天", value: "7d" }, { label: "自定义", value: "custom" }]}
            onChange={choosePreset}
          />
          {datePreset === "custom" ? (
            <Space wrap>
              <label className="query-date-field">开始<input type="date" value={formatQueryTime(draft.from_ms, timeZone).date} onChange={(event) => { const from_ms = calendarDayStart(event.target.value, timeZone); if (Number.isFinite(from_ms)) setDraft((value) => ({ ...value, from_ms })); }} /></label>
              <label className="query-date-field">结束<input type="date" value={formatQueryTime(draft.to_ms - 1, timeZone).date} onChange={(event) => { const to_ms = calendarDayStart(shiftCalendarDate(event.target.value, 1), timeZone); if (Number.isFinite(to_ms)) setDraft((value) => ({ ...value, to_ms })); }} /></label>
            </Space>
          ) : null}
          <Button type="text" icon={<SlidersHorizontal size={16} />} onClick={() => updatePreferences({ advanced: !advanced })}>高级筛选</Button>
          <Space className="query-filter-actions">
            <Button onClick={resetFilters}>重置</Button>
            <Button type="primary" icon={<Search size={16} />} onClick={applyFilters} disabled={!validDateRange}>查询</Button>
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
            <FilterSelect label="排序字段" value={sort} values={["occurred_at", "duration"] as const} onChange={(value) => { updatePreferences({ sort: value ?? "occurred_at" }); resetContext(); }} />
            <FilterSelect label="排序方向" value={order} values={["desc", "asc"] as const} onChange={(value) => { updatePreferences({ order: value ?? "desc" }); resetContext(); }} />
          </div>
        ) : null}
        {!validDateRange ? <Alert type="warning" title="开始日期不能晚于结束日期" /> : null}
      </section>


      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {page ? (
        <section className="query-records" aria-label="解析记录列表">
          <div className="query-records-toolbar">
            <Typography.Text type="secondary" title={timeZone}>{sort === "occurred_at" ? "发生时间" : "总耗时"}{order === "desc" ? "降序" : "升序"} · {timeZoneLabel(timeZone, Date.now(), "offset")}</Typography.Text>
            {autoRefresh && (query.requiresResync || (query.pendingCount ?? 0) > 0) ? <Space wrap>
              <span role="status">{query.requiresResync ? "有新记录，需重新同步" : `有 ${query.pendingCount} 条新记录`}</span>
              <Button size="small" icon={<RotateCw size={15} />} onClick={showLatest}>查看新记录</Button>
            </Space> : null}
          </div>
          <PageState empty={visibleItems.length === 0} emptyDescription="当前筛选条件下没有解析记录" />
          {visibleItems.length > 0 && !narrow ? (
            <Table
              rowKey="id"
              columns={columns}
              dataSource={visibleItems}
              tableLayout="fixed"
              rowClassName={(record) => record.id === detail?.record.id ? "query-row-active" : ""}
              scroll={{ x: 1_132 }}
              pagination={false}
              loading={query.isFetching && !query.isLoading}
            />
          ) : null}
          {narrow ? <div className="query-mobile-records">{visibleItems.map((record) => <article key={record.id} className={`query-record-card${detail?.record.id === record.id ? " query-row-active" : ""}`}>
            <div className="query-card-meta"><span>{formatQueryTime(record.occurred_at_ms, timeZone).date} {formatQueryTime(record.occurred_at_ms, timeZone).time}</span><span className="query-mono">{formatDurationSummary(record).response}</span></div>
            <strong className="query-card-qname">{record.qname}</strong>
            <small>{record.qtype} · {record.transport.toUpperCase()}</small>
            <button type="button" className="query-result-trigger" ref={(node) => { if (node) detailTriggers.current.set(record.id, node); else detailTriggers.current.delete(record.id); }} aria-label={`查看 ${record.qname} 的详情`} aria-haspopup="dialog" aria-expanded={detail?.record.id === record.id} onClick={() => pinDetail(record)}><ResponseCell record={record} /><Eye size={16} aria-hidden /></button>
            <div className="query-card-meta"><span>{formatClientIdentity(record).primary}</span><span>{record.identity.client_ip}</span></div>
          </article>)}</div> : null}
          <div className="query-pagination">
            <Typography.Text type="secondary">当前显示 {visibleItems.length} 条</Typography.Text>
            <Space>
              <Select
                aria-label="每页条数"
                value={pageSize}
                options={[10, 20, 50, 100].map((value) => ({ value, label: `${value} 条/页` }))}
                onChange={(pageSize) => { updatePreferences({ pageSize }); resetContext(); }}
              />
              <Button
                aria-label="上一页"
                title="更新记录"
                icon={<ChevronLeft size={16} />}
                disabled={!page.previous_cursor || query.isFetching}
                onClick={() => { if (detail) suppressedDetailOpen.current = detail.record.id; setDetail(undefined); setDetailPinned(false); setNavigation({ cursor: page.previous_cursor, direction: "newer" }); }}
              />
              <Button
                aria-label="下一页"
                title="更早记录"
                icon={<ChevronRight size={16} />}
                disabled={!page.next_cursor || query.isFetching}
                onClick={() => { if (detail) suppressedDetailOpen.current = detail.record.id; setDetail(undefined); setDetailPinned(false); setNavigation({ cursor: page.next_cursor, direction: "older" }); }}
              />
            </Space>
          </div>
        </section>
      ) : null}
      <Drawer placement="bottom" size="85dvh" open={narrow && !!detail} onClose={() => closeDetail()} aria-labelledby={drawerTitleId} title={<span id={drawerTitleId}>{detail ? `${detail.record.qname} 解析详情` : "解析详情"}</span>} destroyOnHidden className="query-mobile-drawer">
        {narrow && detail ? <QueryDetails key={detail.record.id} snapshot={detail} pinned={detailPinned} onPin={() => setDetailPinned((value) => !value)} onClose={() => closeDetail()} mobile /> : null}
      </Drawer>
    </PageFrame>
    </div>
  );
}

function TimeCell({ value }: { value: number }) {
  const { timeZone } = useServerTimeZone();
  const time = formatQueryTime(value, timeZone);
  return <CellStack primary={time.time} secondary={time.date} />;
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
  const exceptional = record.answers.state !== "available";
  const source = sourceLabel(record);
  return (
    <CellStack
      primary={summary.primary}
      secondary={<><Tag color={source.color}>{source.label}</Tag>{exceptional ? <span>{summary.meta}</span> : null}</>}
    />
  );
}

function QueryDetails({ snapshot, pinned, onPin, onClose, mobile = false }: { snapshot: DetailSnapshot; pinned: boolean; onPin: () => void; onClose: () => void; mobile?: boolean }) {
  const { record } = snapshot;
  const { timeZone } = useServerTimeZone();
  const [copyStatus, setCopyStatus] = useState("");
  const answers = record.answers.state === "unavailable" ? [] : record.answers.records;
  const durations = formatDurationSummary(record);
  const copyDomain = async () => {
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable");
      await navigator.clipboard.writeText(record.qname);
      setCopyStatus("域名已复制");
    } catch {
      setCopyStatus("复制不可用，请选择域名手工复制");
    }
  };
  return (
    <div className="query-details-popover" role={mobile ? "region" : "dialog"} aria-label={`${record.qname} 解析详情`}>
      <div className="query-detail-heading">
        <Typography.Title level={5}>{record.qname}</Typography.Title>
        <Space size={2}>
          <Button type="text" size="small" aria-label="复制域名" icon={<Copy size={15} />} onClick={() => void copyDomain()} />
          {!mobile ? <Button type="text" size="small" aria-label={pinned ? "取消固定" : "固定详情"} aria-pressed={pinned} icon={<Pin size={15} />} onClick={onPin} /> : null}
          <Button type="text" size="small" aria-label="关闭详情" icon={<X size={15} />} onClick={onClose} />
        </Space>
      </div>
      {copyStatus ? <div role="status">{copyStatus}</div> : null}
      <div className="query-detail-meta">{record.qtype} · {record.transport.toUpperCase()} · {formatQueryTimestamp(record.occurred_at_ms, timeZone)} · {pinned ? "点击固定" : "悬停预览"}</div>
      <div className="query-detail-result"><span>{record.rcode} · {sourceLabel(record).label} · {responseStatusLabel(record)}</span><strong className="query-mono">{durations.response}</strong></div>
      {["failed", "cancelled"].includes(record.response_status) ? <Alert type="warning" showIcon title={responseStatusLabel(record)} /> : null}
      <div className="query-answer-list">
        <Typography.Text strong>Answer</Typography.Text>
        {record.answers.state === "unavailable" ? <Typography.Text type="secondary">结果未保留</Typography.Text> : null}
        {record.answers.state === "truncated" ? <Alert type="warning" showIcon title={`保留 ${answers.length} 条，共 ${record.answers.total_count} 条`} /> : null}
        {record.answers.state === "available" && answers.length === 0 ? <Typography.Text type="secondary">{record.rcode === "NOERROR" && record.answers.total_count === 0 ? "NOERROR · NODATA" : "响应没有 Answer 记录"}</Typography.Text> : null}
        {answers.map((answer, index) => <div className="query-answer-row" key={`${answer.name}:${answer.type}:${index}`}>
          <span>{answer.type}</span><div>{answer.name !== record.qname ? <code className="query-answer-owner">{answer.name}</code> : null}<code>{answer.data}</code></div><span>{answer.ttl_seconds}s</span>
        </div>)}
      </div>
      <Descriptions size="small" column={1} colon={false}>
        <Descriptions.Item label={record.source === "cache" ? "缓存来源链路" : "本次应答"}><span className="query-detail-route">{formatRoute(record)}</span></Descriptions.Item>
        {record.cache_activity ? <Descriptions.Item label="后台活动"><CacheActivityTags record={record} />{record.cache_activity.upstream_target_name ? <div className="query-detail-route">刷新链路：{[record.listener_name, record.cache_activity.upstream_target_name, record.cache_activity.upstream_used_name].filter(Boolean).join(" → ")}</div> : null}</Descriptions.Item> : null}
        <Descriptions.Item label="客户端">{formatClientIdentity(record).primary} / {record.identity.client_ip}</Descriptions.Item>
      </Descriptions>
      <QueryEcsSummary ecs={record.ecs} />
      <details className="query-diagnostics">
        <summary>诊断信息</summary>
        <Descriptions size="small" column={1} colon={false}>
          <Descriptions.Item label="原始身份">{record.identity.client_id ?? "未传入 ID"} · {record.identity.client_ip}</Descriptions.Item>
          <Descriptions.Item label="当时匹配">{formatHistoricalMatch(record)}</Descriptions.Item>
          <Descriptions.Item label="当前名称">{record.current_client_name ?? "配置中已无对应名称"}</Descriptions.Item>
          <Descriptions.Item label="总耗时">{durations.total}</Descriptions.Item>
          <Descriptions.Item label="主链耗时">{durations.dnsCore}</Descriptions.Item>
          <Descriptions.Item label="响应耗时"><span tabIndex={0} title="收到完整请求至服务端成功写出响应，不含后台刷新，也不是客户端接收确认">{durations.response}</span></Descriptions.Item>
          <Descriptions.Item label="发生时间">{formatQueryTimestamp(record.occurred_at_ms, timeZone)}</Descriptions.Item>
          <Descriptions.Item label="记录 ID"><code>{record.id}</code></Descriptions.Item>
          <Descriptions.Item label="目录快照">{snapshot.directoryRevision}</Descriptions.Item>
        </Descriptions>
        <QueryEcsDiagnostics ecs={record.ecs} />
      </details>
    </div>
  );
}

/** 列表与详情共用服务端时区；UTC 默认值仅供独立格式化调用。 */
export function formatQueryTime(value: number, timeZone = "UTC"): { date: string; time: string } {
  return formatTimeParts(value, timeZone);
}

function formatQueryTimestamp(value: number, timeZone: string): string {
  const parts = formatQueryTime(value, timeZone);
  if (parts.date === "—") return "未记录";
  return `${parts.date} ${parts.time}.${String(new Date(value).getUTCMilliseconds()).padStart(3, "0")} ${timeZoneLabel(timeZone, value)}`;
}

function responseDuration(record: QueryRecord): string {
  return record.response_duration_us == null ? "未记录" : formatDuration(record.response_duration_us / 1_000);
}

/** 使用真实媒体查询切换呈现；Drawer 与浮层继续读取同一份固定快照。 */
function useNarrowQueries(): boolean {
  const [narrow, setNarrow] = useState(() => window.matchMedia("(max-width: 760px)").matches);
  useEffect(() => {
    const media = window.matchMedia("(max-width: 760px)");
    const update = () => setNarrow(media.matches);
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return narrow;
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
  const result = formatCacheActivityResult(activity);
  return <>{activity.kind === "refresh" ? <Tag color="blue">后台刷新</Tag> : null}
    <Tag color={activity.outcome === "inserted" ? "green" : activity.outcome === "updated" ? "blue" : ["failed", "dropped"].includes(activity.outcome) ? "orange" : "default"}>{labels[activity.outcome]}{result ? ` · ${result}` : null}</Tag></>;
}

/**
 * 写入/刷新拿到的响应类与写入 TTL，例如 `NODATA 300s`；
 * 用于区分“刷新成功但仍是空应答”与真正拿到新记录。旧记录两字段均为 null 时不展示。
 */
export function formatCacheActivityResult(activity: NonNullable<QueryRecord["cache_activity"]>): string | null {
  const classLabels: Record<NonNullable<typeof activity.response_class>, string | null> = {
    positive: null, nodata: "NODATA", nxdomain: "NXDOMAIN", servfail: "SERVFAIL", truncated: "TC", refused: "REFUSED", other: "其他",
  };
  const classLabel = activity.response_class ? classLabels[activity.response_class] : null;
  const ttl = activity.ttl_secs === null || activity.ttl_secs === undefined ? null : `${activity.ttl_secs}s`;
  if (classLabel && ttl) return `${classLabel} ${ttl}`;
  if (classLabel) return classLabel;
  return ttl ? `TTL ${ttl}` : null;
}

/** 本地来源不产生写入结果，用同级标签补上路由列标签行，避免与缓存标签行排版不一致。 */
function RouteSourceTag({ record }: { record: QueryRecord }) {
  return record.source === "hosts" ? <Tag color="purple">Hosts</Tag> : null;
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
  // NOERROR 且明确保留了 0 条 Answer 才是空应答；截断/未保留时无法判断，仍按 outcome 展示。
  const nodata = !answer && record.rcode === "NOERROR" && record.answers.state === "available" && record.answers.total_count === 0;
  return {
    primary: answer ? `${answer.type}  ${answer.data}` : nodata ? "NOERROR · NODATA" : `${record.rcode} · ${record.outcome}`,
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
  const { timeZone } = useServerTimeZone();
  if (!enabled) return <Typography.Text type="secondary">已关闭</Typography.Text>;
  const label = state === "open" ? "实时连接正常" : state === "connecting" ? "连接中" : state === "reconnecting" ? "正在重连" : "实时连接异常";
  return (
    <Typography.Text type={state === "error" ? "danger" : "secondary"} className={`query-connection query-connection-${state}`} role="status" title={timeZone}>
      {label}{lastUpdatedAt ? ` · ${formatQueryTime(lastUpdatedAt, timeZone).time} ${timeZoneLabel(timeZone, lastUpdatedAt, "offset")}` : ""}
    </Typography.Text>
  );
}

function defaultFilter(timeZone: string, now = Date.now()): QueryFilter {
  return calendarDayRange(now, 7, timeZone);
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
